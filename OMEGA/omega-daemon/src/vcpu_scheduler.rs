//! Gestionnaire de slots vCPU — modèle élastique avec partage de file d'instructions.
//!
//! # Modèle vCPU
//!
//! ```text
//! Nœud physique
//! ├── pCPU 0  [slot 0, slot 1, slot 2]   ← 3 vCPU max par pCPU
//! ├── pCPU 1  [slot 0, slot 1, slot 2]
//! └── pCPU N  [slot 0, slot 1, slot 2]
//!
//! VM alice  : min=2 vCPU, max=4 vCPU, actuel=2
//! VM bob    : min=1 vCPU, max=2 vCPU, actuel=1
//! VM carol  : min=4 vCPU, max=8 vCPU, actuel=4
//! ```
//!
//! # Élasticité
//!
//! Chaque VM est créée avec un `min_vcpus` (démarrage) et un `max_vcpus`
//! (plafond demandé). Le daemon hotplug des vCPU supplémentaires quand :
//!   - La VM dépasse 80% d'utilisation CPU sur ses vCPU actuels
//!   - Des slots pCPU sont disponibles sur le nœud
//!
//! # Partage de file (instruction sharing)
//!
//! Un slot vCPU peut être partagé entre plusieurs VMs (max `MAX_VMS_PER_SLOT`).
//! En pratique, le scheduler CFS du kernel gère le time-slicing — nous gérons
//! uniquement *quelles VMs* partagent *quel slot* et combien de VMs max.
//!
//! Quand un slot a atteint `MAX_VMS_PER_SLOT` VMs et qu'une nouvelle VM demande
//! un vCPU supplémentaire → décision : **migrer la VM** plutôt que de surcharger.
//!
//! # Steal time
//!
//! Le steal time (temps CPU volé à la VM par l'hyperviseur) est lu depuis
//! `/proc/stat` (champ `steal` de chaque CPU). Un steal > `STEAL_THRESHOLD_PCT`
//! indique un nœud surchargé → déclenche une migration prioritaire.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::{debug, info, warn};

/// Nombre maximum de vCPUs virtuels par cœur physique.
pub const VCPU_PER_PCPU: usize = 3;

/// Nombre maximum de VMs partageant un même slot vCPU.
pub const MAX_VMS_PER_SLOT: usize = 3;

/// Seuil de steal time déclenchant une migration prioritaire (%).
pub const STEAL_THRESHOLD_PCT: f64 = 10.0;

/// Seuil d'utilisation CPU déclenchant un hotplug de vCPU (%).
pub const HOTPLUG_TRIGGER_PCT: f64 = 80.0;

/// Seuil d'utilisation CPU sous lequel on peut envisager un retrait progressif.
pub const DOWNSCALE_TRIGGER_PCT: f64 = 35.0;

/// Durée minimale de faible charge avant de retirer un vCPU.
pub const DOWNSCALE_STABLE_SECS: u64 = 60;

/// Poids CPU nominal appliqué aux VMs sans partage local particulier.
pub const DEFAULT_CPU_WEIGHT: u32 = 100;

/// Poids CPU d'une VM sous pression à qui on donne temporairement la priorité.
pub const BOOSTED_CPU_WEIGHT: u32 = 200;

/// Poids CPU d'une VM durablement idle qui cède temporairement de la priorité.
pub const DONOR_CPU_WEIGHT: u32 = 50;

/// Seuil maximal de steal/throttle toléré pour une VM donneuse locale (%).
pub const DONOR_SAFE_STEAL_PCT: f64 = 5.0;

// ─── Types ────────────────────────────────────────────────────────────────────

/// Identifiant d'un slot vCPU : (pcpu_id, slot_index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct SlotId {
    pub pcpu: usize,
    pub slot: usize,
}

impl SlotId {
    pub fn new(pcpu: usize, slot: usize) -> Self {
        Self { pcpu, slot }
    }
}

/// État élastique d'une VM.
#[derive(Debug, Clone, Serialize)]
pub struct VmVcpuState {
    pub vm_id: u32,
    /// vCPUs minimum (démarrage)
    pub min_vcpus: usize,
    /// vCPUs maximum (plafond demandé par l'utilisateur)
    pub max_vcpus: usize,
    /// vCPUs actuellement alloués (min ≤ current ≤ max)
    pub current_vcpus: usize,
    /// Slots pCPU assignés à cette VM
    pub slots: Vec<SlotId>,
    /// Utilisation CPU moyenne sur les 60 dernières secondes (%)
    pub cpu_usage_pct: f64,
    /// Steal time détecté (%)
    pub steal_pct: f64,
    /// Poids CPU cgroup actuellement demandé pour cette VM.
    pub cpu_weight: u32,
    /// La VM participe-t-elle à un partage CPU local temporaire ?
    pub local_share_active: bool,
    /// Depuis quand la VM est en faible charge prolongée.
    pub low_load_since: Option<u64>,
    /// Timestamp de la dernière mise à jour
    pub updated_at: u64,
}

impl VmVcpuState {
    pub fn new(vm_id: u32, min_vcpus: usize, max_vcpus: usize) -> Self {
        Self {
            vm_id,
            min_vcpus,
            max_vcpus,
            current_vcpus: min_vcpus,
            slots: Vec::new(),
            cpu_usage_pct: 0.0,
            steal_pct: 0.0,
            cpu_weight: DEFAULT_CPU_WEIGHT,
            local_share_active: false,
            low_load_since: None,
            updated_at: now_secs(),
        }
    }

    /// La VM a-t-elle atteint son plafond de vCPUs ?
    pub fn at_max_vcpus(&self) -> bool {
        self.current_vcpus >= self.max_vcpus
    }

    /// La VM est-elle sous pression CPU (dépasse le seuil de hotplug) ?
    pub fn needs_more_vcpus(&self) -> bool {
        self.cpu_usage_pct >= HOTPLUG_TRIGGER_PCT && !self.at_max_vcpus()
    }

    /// La VM souffre-t-elle de steal time excessif ?
    pub fn has_steal_pressure(&self) -> bool {
        self.steal_pct >= STEAL_THRESHOLD_PCT
    }

    /// La VM peut-elle perdre 1 vCPU sans passer sous son minimum ?
    pub fn can_downscale(&self) -> bool {
        self.current_vcpus > self.safe_vcpu_floor()
            && self.cpu_usage_pct <= DOWNSCALE_TRIGGER_PCT
            && self.low_load_duration_secs() >= DOWNSCALE_STABLE_SECS
    }

    /// La VM peut-elle servir de donneuse CPU locale ?
    pub fn can_lend_cpu_locally(&self) -> bool {
        self.can_downscale()
            && self.steal_pct <= DONOR_SAFE_STEAL_PCT
            && !self.has_steal_pressure()
            && !self.local_share_active
    }

    /// Plancher dynamique en dessous duquel la VM ne doit pas descendre.
    ///
    /// Il reste au minimum déclaré, mais peut monter si l'utilisation CPU récente
    /// suggère que la VM a encore besoin de plus de parallélisme pour survivre
    /// correctement après un retrait.
    pub fn safe_vcpu_floor(&self) -> usize {
        let usage_floor = ((self.cpu_usage_pct / DOWNSCALE_TRIGGER_PCT).ceil() as usize).max(1);
        self.min_vcpus
            .max(usage_floor)
            .min(self.current_vcpus.max(1))
    }

    /// VM sous pression qui mérite un partage CPU local temporaire.
    pub fn needs_local_cpu_share(&self) -> bool {
        (self.needs_more_vcpus() || self.has_steal_pressure()) && self.cpu_usage_pct > 0.0
    }

    pub fn vcpu_deficit(&self) -> usize {
        self.max_vcpus.saturating_sub(self.current_vcpus)
    }

    pub fn low_load_duration_secs(&self) -> u64 {
        self.low_load_since
            .map(|started| now_secs().saturating_sub(started))
            .unwrap_or(0)
    }
}

// ─── Registre des slots ───────────────────────────────────────────────────────

/// Un slot vCPU et les VMs qui le partagent.
#[derive(Debug, Default)]
struct PcpuSlot {
    /// VMs assignées à ce slot (max MAX_VMS_PER_SLOT)
    vms: HashSet<u32>,
}

impl PcpuSlot {
    fn can_accept(&self) -> bool {
        self.vms.len() < MAX_VMS_PER_SLOT
    }
}

// ─── VcpuScheduler ───────────────────────────────────────────────────────────

/// Résultat d'une décision d'allocation vCPU.
#[derive(Debug, Clone)]
pub enum VcpuDecision {
    /// Slots alloués avec succès.
    Allocated { vm_id: u32, slots: Vec<SlotId> },
    /// Un vCPU supplémentaire a été hotplugué.
    Hotplugged {
        vm_id: u32,
        new_count: usize,
        slot: SlotId,
    },
    /// Plus de slots disponibles → migration recommandée.
    MigrateRequired { vm_id: u32, reason: String },
    /// La VM est déjà à son maximum.
    AtMax { vm_id: u32 },
    /// La VM est déjà à son minimum.
    AtMin { vm_id: u32 },
    /// Un vCPU a été retiré progressivement.
    Downscaled {
        vm_id: u32,
        new_count: usize,
        slot: SlotId,
    },
}

/// Scheduler de vCPU — gère les slots pCPU et l'élasticité des VMs.
pub struct VcpuScheduler {
    /// Nombre de CPUs physiques du nœud
    num_pcpus: usize,
    /// Slots : [pcpu_id][slot_index]
    slots: RwLock<Vec<Vec<PcpuSlot>>>,
    /// États des VMs
    vms: RwLock<HashMap<u32, VmVcpuState>>,
}

impl VcpuScheduler {
    /// Crée un scheduler pour un nœud avec `num_pcpus` cœurs physiques.
    pub fn new(num_pcpus: usize) -> Arc<Self> {
        let slots = (0..num_pcpus)
            .map(|_| (0..VCPU_PER_PCPU).map(|_| PcpuSlot::default()).collect())
            .collect();

        info!(
            num_pcpus,
            total_vslots = num_pcpus * VCPU_PER_PCPU,
            max_vms_per_slot = MAX_VMS_PER_SLOT,
            "VcpuScheduler initialisé"
        );

        Arc::new(Self {
            num_pcpus,
            slots: RwLock::new(slots),
            vms: RwLock::new(HashMap::new()),
        })
    }

    // ─── Admission ────────────────────────────────────────────────────────

    /// Enregistre une nouvelle VM et lui alloue ses `min_vcpus` slots.
    ///
    /// Retourne `VcpuDecision::MigrateRequired` si le nœud n'a plus de place.
    pub fn admit_vm(&self, vm_id: u32, min_vcpus: usize, max_vcpus: usize) -> VcpuDecision {
        let mut vms = self.vms.write().unwrap();
        let mut slots = self.slots.write().unwrap();

        // Trouver min_vcpus slots libres
        let mut allocated: Vec<SlotId> = Vec::new();

        'outer: for pcpu in 0..self.num_pcpus {
            for slot in 0..VCPU_PER_PCPU {
                if allocated.len() >= min_vcpus {
                    break 'outer;
                }
                if slots[pcpu][slot].can_accept() {
                    slots[pcpu][slot].vms.insert(vm_id);
                    allocated.push(SlotId::new(pcpu, slot));
                }
            }
        }

        if allocated.len() < min_vcpus {
            // Libérer ce qu'on a partiellement alloué
            for sid in &allocated {
                slots[sid.pcpu][sid.slot].vms.remove(&vm_id);
            }
            return VcpuDecision::MigrateRequired {
                vm_id,
                reason: format!(
                    "nœud saturé : seulement {} slots disponibles sur {} demandés",
                    allocated.len(),
                    min_vcpus
                ),
            };
        }

        let mut state = VmVcpuState::new(vm_id, min_vcpus, max_vcpus);
        state.slots = allocated.clone();

        info!(
            vm_id,
            min_vcpus,
            max_vcpus,
            slots_allocated = allocated.len(),
            "VM admise dans le scheduler vCPU"
        );

        vms.insert(vm_id, state);
        VcpuDecision::Allocated {
            vm_id,
            slots: allocated,
        }
    }

    /// Libère tous les slots d'une VM (arrêt ou migration).
    pub fn release_vm(&self, vm_id: u32) {
        let mut vms = self.vms.write().unwrap();
        let mut slots = self.slots.write().unwrap();

        if let Some(state) = vms.remove(&vm_id) {
            for sid in &state.slots {
                slots[sid.pcpu][sid.slot].vms.remove(&vm_id);
            }
            info!(
                vm_id,
                released_slots = state.slots.len(),
                "VM retirée du scheduler"
            );
        }
    }

    /// Met à jour le profil min/max d'une VM déjà suivie.
    ///
    /// Le nouveau minimum ne peut pas dépasser le nombre de vCPUs déjà en ligne.
    /// Pour augmenter réellement `current_vcpus`, utiliser le hotplug QMP.
    pub fn update_profile(
        &self,
        vm_id: u32,
        min_vcpus: usize,
        max_vcpus: usize,
    ) -> Result<(), String> {
        if min_vcpus == 0 {
            return Err("min_vcpus doit être ≥ 1".into());
        }
        if max_vcpus < min_vcpus {
            return Err(format!("max_vcpus ({max_vcpus}) < min_vcpus ({min_vcpus})"));
        }

        let mut vms = self.vms.write().unwrap();
        let Some(state) = vms.get_mut(&vm_id) else {
            return Err(format!("VM {vm_id} inconnue"));
        };

        if max_vcpus < state.current_vcpus {
            return Err(format!(
                "max_vcpus ({max_vcpus}) < current_vcpus ({})",
                state.current_vcpus
            ));
        }
        if min_vcpus > state.current_vcpus {
            return Err(format!(
                "min_vcpus ({min_vcpus}) > current_vcpus ({})",
                state.current_vcpus
            ));
        }

        state.min_vcpus = min_vcpus;
        state.max_vcpus = max_vcpus;
        state.updated_at = now_secs();

        info!(
            vm_id,
            min_vcpus,
            max_vcpus,
            current_vcpus = state.current_vcpus,
            "profil vCPU mis à jour"
        );
        Ok(())
    }

    // ─── Élasticité ───────────────────────────────────────────────────────

    /// Tente de hotpluguer un vCPU supplémentaire pour une VM.
    ///
    /// Appelé quand `cpu_usage_pct ≥ HOTPLUG_TRIGGER_PCT`.
    pub fn try_hotplug(&self, vm_id: u32) -> VcpuDecision {
        let mut vms = self.vms.write().unwrap();
        let mut slots = self.slots.write().unwrap();

        let Some(state) = vms.get_mut(&vm_id) else {
            return VcpuDecision::MigrateRequired {
                vm_id,
                reason: "VM inconnue".into(),
            };
        };

        if state.at_max_vcpus() {
            debug!(vm_id, max = state.max_vcpus, "VM déjà à son max vCPU");
            return VcpuDecision::AtMax { vm_id };
        }

        // Chercher un slot libre (préférence : même pCPU pour localité)
        let preferred_pcpus: Vec<usize> = state.slots.iter().map(|s| s.pcpu).collect();
        let all_pcpus: Vec<usize> = (0..self.num_pcpus).collect();

        let search_order: Vec<usize> = preferred_pcpus
            .into_iter()
            .chain(all_pcpus)
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        for pcpu in search_order {
            for slot in 0..VCPU_PER_PCPU {
                if slots[pcpu][slot].can_accept() {
                    slots[pcpu][slot].vms.insert(vm_id);
                    let sid = SlotId::new(pcpu, slot);
                    state.slots.push(sid);
                    let new_count = state.current_vcpus + 1;
                    state.current_vcpus = new_count;
                    state.updated_at = now_secs();

                    info!(vm_id, new_count, pcpu, slot, "vCPU hotplugué");
                    return VcpuDecision::Hotplugged {
                        vm_id,
                        new_count,
                        slot: sid,
                    };
                }
            }
        }

        // Plus de slots disponibles
        warn!(
            vm_id,
            current = state.current_vcpus,
            max = state.max_vcpus,
            "hotplug impossible — nœud saturé → migration recommandée"
        );

        VcpuDecision::MigrateRequired {
            vm_id,
            reason: format!(
                "plus de slots vCPU disponibles ({}/{} vCPU alloués, max demandé {})",
                state.current_vcpus,
                self.total_vslots(),
                state.max_vcpus,
            ),
        }
    }

    /// Tente de retirer 1 vCPU à une VM.
    ///
    /// `force=true` permet la convergence initiale vers le minimum au démarrage,
    /// sans attendre la fenêtre de faible charge.
    pub fn try_downscale(&self, vm_id: u32, force: bool) -> VcpuDecision {
        let mut vms = self.vms.write().unwrap();
        let mut slots = self.slots.write().unwrap();

        let Some(state) = vms.get_mut(&vm_id) else {
            return VcpuDecision::AtMin { vm_id };
        };

        if state.current_vcpus <= state.min_vcpus {
            return VcpuDecision::AtMin { vm_id };
        }

        if !force && !state.can_downscale() {
            return VcpuDecision::AtMin { vm_id };
        }

        let Some(slot) = state.slots.pop() else {
            return VcpuDecision::AtMin { vm_id };
        };

        slots[slot.pcpu][slot.slot].vms.remove(&vm_id);
        state.current_vcpus = state.current_vcpus.saturating_sub(1);
        state.updated_at = now_secs();
        if state.current_vcpus <= state.min_vcpus {
            state.low_load_since = None;
        }

        let new_count = state.current_vcpus;
        info!(
            vm_id,
            new_count,
            pcpu = slot.pcpu,
            slot = slot.slot,
            "vCPU retiré du scheduler"
        );
        VcpuDecision::Downscaled {
            vm_id,
            new_count,
            slot,
        }
    }

    /// Annule un hotplug précédemment réservé si l'opération réelle échoue.
    pub fn rollback_hotplug(&self, vm_id: u32, slot: SlotId) -> bool {
        let mut vms = self.vms.write().unwrap();
        let mut slots = self.slots.write().unwrap();

        let Some(state) = vms.get_mut(&vm_id) else {
            return false;
        };

        let Some(pos) = state.slots.iter().rposition(|sid| *sid == slot) else {
            return false;
        };

        state.slots.remove(pos);
        state.current_vcpus = state.current_vcpus.saturating_sub(1);
        state.updated_at = now_secs();
        slots[slot.pcpu][slot.slot].vms.remove(&vm_id);
        true
    }

    /// Ré-annule un retrait si le hot-unplug réel échoue.
    pub fn rollback_downscale(&self, vm_id: u32, slot: SlotId) -> bool {
        let mut vms = self.vms.write().unwrap();
        let mut slots = self.slots.write().unwrap();

        let Some(state) = vms.get_mut(&vm_id) else {
            return false;
        };

        slots[slot.pcpu][slot.slot].vms.insert(vm_id);
        state.slots.push(slot);
        state.current_vcpus += 1;
        state.updated_at = now_secs();
        true
    }

    // ─── Monitoring ───────────────────────────────────────────────────────

    /// Met à jour les métriques CPU d'une VM (usage, steal).
    pub fn update_vm_metrics(&self, vm_id: u32, cpu_usage_pct: f64, steal_pct: f64) {
        let mut vms = self.vms.write().unwrap();
        if let Some(state) = vms.get_mut(&vm_id) {
            state.cpu_usage_pct = cpu_usage_pct;
            state.steal_pct = steal_pct;
            if state.current_vcpus > state.min_vcpus && cpu_usage_pct <= DOWNSCALE_TRIGGER_PCT {
                if state.low_load_since.is_none() {
                    state.low_load_since = Some(now_secs());
                }
            } else {
                state.low_load_since = None;
            }
            state.updated_at = now_secs();

            if state.has_steal_pressure() {
                warn!(
                    vm_id,
                    steal_pct,
                    "steal time élevé — nœud surchargé, migration prioritaire recommandée"
                );
            }
        }
    }

    /// Lit le steal time global du nœud depuis /proc/stat.
    ///
    /// Sur machine physique : mesure l'overhead des autres processus hôte.
    /// Sur VM Proxmox (lab KVM) : mesure le temps volé par l'hyperviseur.
    ///
    /// Pour des métriques par VM (précises), utiliser `CgroupCpuController::read_cpu_stat`.
    pub fn read_node_steal_pct(&self) -> f64 {
        read_steal_from_proc_stat().unwrap_or(0.0)
    }

    /// Met à jour les métriques d'une VM depuis son cgroup v2.
    ///
    /// Plus précis que update_vm_metrics() car les données viennent
    /// directement du kernel (pas d'interpolation).
    pub fn update_from_cgroup(&self, vm_id: u32, usage_pct: f64, throttle_ratio: f64) {
        // Le steal time par VM n'existe pas sur physique (c'est un concept
        // hyperviseur). On utilise le throttle_ratio comme proxy : une VM
        // throttlée est une VM qui a besoin de plus de CPU qu'on ne lui donne.
        let effective_steal = throttle_ratio * 100.0;
        self.update_vm_metrics(vm_id, usage_pct, effective_steal);
    }

    /// VMs qui ont besoin d'un hotplug (usage > seuil, pas au max).
    pub fn vms_needing_hotplug(&self) -> Vec<u32> {
        self.vms
            .read()
            .unwrap()
            .values()
            .filter(|s| s.needs_more_vcpus())
            .map(|s| s.vm_id)
            .collect()
    }

    /// VMs qui peuvent perdre 1 vCPU après une période de faible charge.
    pub fn vms_needing_downscale(&self) -> Vec<u32> {
        self.vms
            .read()
            .unwrap()
            .values()
            .filter(|s| s.can_downscale())
            .map(|s| s.vm_id)
            .collect()
    }

    /// VMs candidates à la migration (steal élevé ou nœud saturé).
    pub fn vms_needing_migration(&self) -> Vec<(u32, String)> {
        self.vms
            .read()
            .unwrap()
            .values()
            .filter(|s| s.has_steal_pressure())
            .map(|s| (s.vm_id, format!("steal {:.1}%", s.steal_pct)))
            .collect()
    }

    /// VMs sous pression à servir d'abord localement, triées par déficit croissant.
    pub fn local_share_borrowers(&self) -> Vec<u32> {
        let mut borrowers: Vec<_> = self
            .vms
            .read()
            .unwrap()
            .values()
            .filter(|s| s.needs_local_cpu_share())
            .map(|s| (s.vm_id, s.vcpu_deficit(), s.steal_pct, s.cpu_usage_pct))
            .collect();

        borrowers.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| b.3.total_cmp(&a.3))
        });

        borrowers
            .into_iter()
            .map(|(vm_id, _, _, _)| vm_id)
            .collect()
    }

    /// VMs durablement au repos pouvant céder 1 vCPU réel.
    pub fn local_share_donors(&self) -> Vec<u32> {
        let mut donors: Vec<_> = self
            .vms
            .read()
            .unwrap()
            .values()
            .filter(|s| s.can_lend_cpu_locally())
            .map(|s| {
                (
                    s.vm_id,
                    s.current_vcpus.saturating_sub(s.safe_vcpu_floor()),
                    s.low_load_duration_secs(),
                    s.cpu_usage_pct,
                )
            })
            .collect();

        donors.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.3.total_cmp(&b.3))
        });

        donors.into_iter().map(|(vm_id, _, _, _)| vm_id).collect()
    }

    /// VMs qui ne cèdent pas forcément 1 vCPU, mais dont la priorité CPU peut baisser.
    pub fn local_share_idle_peers(&self) -> Vec<u32> {
        let mut peers: Vec<_> = self
            .vms
            .read()
            .unwrap()
            .values()
            .filter(|s| s.cpu_usage_pct <= DOWNSCALE_TRIGGER_PCT && !s.needs_local_cpu_share())
            .map(|s| (s.vm_id, s.low_load_duration_secs(), s.cpu_usage_pct))
            .collect();

        peers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.total_cmp(&b.2)));
        peers.into_iter().map(|(vm_id, _, _)| vm_id).collect()
    }

    pub fn set_cpu_weight(&self, vm_id: u32, weight: u32, local_share_active: bool) -> bool {
        let mut vms = self.vms.write().unwrap();
        let Some(state) = vms.get_mut(&vm_id) else {
            return false;
        };

        let weight = weight.clamp(1, 10000);
        if state.cpu_weight == weight && state.local_share_active == local_share_active {
            return false;
        }

        state.cpu_weight = weight;
        state.local_share_active = local_share_active;
        state.updated_at = now_secs();
        true
    }

    // ─── Snapshot / métriques ─────────────────────────────────────────────

    /// Nombre de slots vCPU totaux sur le nœud.
    pub fn total_vslots(&self) -> usize {
        self.num_pcpus * VCPU_PER_PCPU
    }

    /// Nombre de slots vCPU encore disponibles.
    pub fn free_vslots(&self) -> usize {
        let slots = self.slots.read().unwrap();
        slots.iter().flatten().filter(|s| s.can_accept()).count()
    }

    /// Taux d'occupation global (0.0 – 1.0).
    pub fn occupancy_ratio(&self) -> f64 {
        let total = self.total_vslots();
        let free = self.free_vslots();
        if total == 0 {
            return 0.0;
        }
        1.0 - (free as f64 / total as f64)
    }

    /// Snapshot de l'état de toutes les VMs.
    pub fn vm_snapshot(&self) -> Vec<VmVcpuState> {
        self.vms.read().unwrap().values().cloned().collect()
    }

    pub fn get_vm_state(&self, vm_id: u32) -> Option<VmVcpuState> {
        self.vms.read().unwrap().get(&vm_id).cloned()
    }

    pub fn has_vm(&self, vm_id: u32) -> bool {
        self.vms.read().unwrap().contains_key(&vm_id)
    }

    /// Résumé pour les métriques Prometheus.
    pub fn prometheus_metrics(&self, node_id: &str) -> String {
        let total = self.total_vslots();
        let free = self.free_vslots();
        let used = total - free;
        let steal = self.read_node_steal_pct();
        let vms = self.vms.read().unwrap();
        let vm_count = vms.len();
        let shared_vm_count = vms.values().filter(|vm| vm.local_share_active).count();

        format!(
            "# HELP omega_vcpu_slots_total Slots vCPU totaux\n\
             omega_vcpu_slots_total{{node=\"{node}\"}} {total}\n\
             # HELP omega_vcpu_slots_used Slots vCPU utilisés\n\
             omega_vcpu_slots_used{{node=\"{node}\"}} {used}\n\
             # HELP omega_vcpu_slots_free Slots vCPU libres\n\
             omega_vcpu_slots_free{{node=\"{node}\"}} {free}\n\
             # HELP omega_vcpu_steal_pct Steal time CPU nœud (%)\n\
             omega_vcpu_steal_pct{{node=\"{node}\"}} {steal:.2}\n\
             # HELP omega_vcpu_vm_count VMs gérées par le scheduler\n\
             omega_vcpu_vm_count{{node=\"{node}\"}} {vm_count}\n\
             # HELP omega_vcpu_local_share_vms VMs actuellement en partage CPU local\n\
             omega_vcpu_local_share_vms{{node=\"{node}\"}} {shared_vm_count}\n",
            node = node_id,
        )
    }
}

// ─── Lecture /proc/stat ───────────────────────────────────────────────────────

fn read_steal_from_proc_stat() -> Option<f64> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().find(|l| l.starts_with("cpu "))?;

    // Format: cpu user nice system idle iowait irq softirq steal guest guest_nice
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();

    if fields.len() < 8 {
        return None;
    }

    let total = fields.iter().sum::<u64>();
    let steal = fields[7]; // champ steal

    if total == 0 {
        return Some(0.0);
    }
    Some((steal as f64 / total as f64) * 100.0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_scheduler(pcpus: usize) -> Arc<VcpuScheduler> {
        VcpuScheduler::new(pcpus)
    }

    #[test]
    fn test_total_slots_is_pcpus_times_3() {
        let s = make_scheduler(4);
        assert_eq!(s.total_vslots(), 12);
    }

    #[test]
    fn test_admit_vm_allocates_min_vcpus() {
        // Un slot accepte jusqu'à MAX_VMS_PER_SLOT (3) VMs.
        // free_vslots() = nombre de slots pouvant encore accueillir une VM.
        // Pour qu'un slot soit saturé il faut y placer 3 VMs (min=1 chacune).
        let s = make_scheduler(4); // 12 slots
        s.admit_vm(1, 1, 2);
        s.admit_vm(2, 1, 2);
        s.admit_vm(3, 1, 2); // slot[0][0] plein (3 VMs = MAX_VMS_PER_SLOT)
        let d = s.admit_vm(4, 1, 2);
        assert!(matches!(d, VcpuDecision::Allocated { .. }));
        // 1 slot saturé sur 12 → 11 libres
        assert_eq!(s.free_vslots(), 12 - 1);
    }

    #[test]
    fn test_admit_multiple_vms_same_slots() {
        // 3 VMs sur 1 pCPU, 1 slot : toutes partagent le même slot
        let s = make_scheduler(1); // 1 pCPU = 3 slots
        s.admit_vm(1, 1, 2);
        s.admit_vm(2, 1, 2);
        s.admit_vm(3, 1, 2);
        // 4ème VM sur le même slot → dépassement MAX_VMS_PER_SLOT
        // mais il reste 2 autres slots sur le même pCPU
        let d = s.admit_vm(4, 1, 2);
        assert!(matches!(d, VcpuDecision::Allocated { .. }));
    }

    #[test]
    fn test_migrate_required_when_no_slots() {
        let s = make_scheduler(1); // 3 slots, 3 VMs chacune prend les 3 max
                                   // Remplir tous les slots : 3 slots × 3 VMs max = 9 admissions
        for i in 1u32..=9 {
            let _ = s.admit_vm(i, 1, 2);
        }
        // La 10ème doit être refusée
        let d = s.admit_vm(10, 1, 2);
        assert!(matches!(d, VcpuDecision::MigrateRequired { .. }));
    }

    #[test]
    fn test_release_frees_slots() {
        // Remplir complètement 1 slot : 3 VMs dans slot[0][0] → 5 slots libres sur 6.
        let s = make_scheduler(2); // 6 slots
        s.admit_vm(1, 1, 2);
        s.admit_vm(2, 1, 2);
        s.admit_vm(3, 1, 2); // slot[0][0] saturé
        assert_eq!(s.free_vslots(), 5); // 1 saturé → 5 libres

        s.release_vm(1);
        s.release_vm(2);
        s.release_vm(3);
        assert_eq!(s.free_vslots(), 6); // tout libéré
    }

    #[test]
    fn test_hotplug_increases_vcpu_count() {
        let s = make_scheduler(4);
        s.admit_vm(1, 2, 4);
        s.update_vm_metrics(1, 85.0, 0.0); // > HOTPLUG_TRIGGER_PCT

        let vms_to_hotplug = s.vms_needing_hotplug();
        assert!(vms_to_hotplug.contains(&1));

        let d = s.try_hotplug(1);
        assert!(matches!(d, VcpuDecision::Hotplugged { new_count: 3, .. }));
    }

    #[test]
    fn test_hotplug_stops_at_max_vcpus() {
        let s = make_scheduler(4);
        s.admit_vm(1, 2, 2); // min=max=2 → pas d'élasticité

        let d = s.try_hotplug(1);
        assert!(matches!(d, VcpuDecision::AtMax { .. }));
    }

    #[test]
    fn test_downscale_requires_stable_low_load() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        let _ = s.try_hotplug(1);
        s.update_vm_metrics(1, DOWNSCALE_TRIGGER_PCT + 10.0, 0.0);
        let d = s.try_downscale(1, false);
        assert!(matches!(d, VcpuDecision::AtMin { .. }));
    }

    #[test]
    fn test_downscale_removes_one_vcpu_after_stable_low_load() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        let _ = s.try_hotplug(1);
        {
            let mut vms = s.vms.write().unwrap();
            let state = vms.get_mut(&1).unwrap();
            state.cpu_usage_pct = DOWNSCALE_TRIGGER_PCT - 5.0;
            state.low_load_since = Some(now_secs().saturating_sub(DOWNSCALE_STABLE_SECS + 1));
        }
        let d = s.try_downscale(1, false);
        assert!(matches!(d, VcpuDecision::Downscaled { new_count: 1, .. }));
    }

    #[test]
    fn test_vms_needing_downscale_are_reported() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        let _ = s.try_hotplug(1);
        {
            let mut vms = s.vms.write().unwrap();
            let state = vms.get_mut(&1).unwrap();
            state.cpu_usage_pct = DOWNSCALE_TRIGGER_PCT - 5.0;
            state.low_load_since = Some(now_secs().saturating_sub(DOWNSCALE_STABLE_SECS + 1));
        }
        assert_eq!(s.vms_needing_downscale(), vec![1]);
    }

    #[test]
    fn test_update_profile_changes_bounds_without_changing_current() {
        let s = make_scheduler(4);
        s.admit_vm(1, 2, 4);

        s.update_profile(1, 1, 6).unwrap();

        let state = s.get_vm_state(1).unwrap();
        assert_eq!(state.min_vcpus, 1);
        assert_eq!(state.max_vcpus, 6);
        assert_eq!(state.current_vcpus, 2);
    }

    #[test]
    fn test_update_profile_rejects_min_above_current() {
        let s = make_scheduler(4);
        s.admit_vm(1, 2, 4);

        let err = s.update_profile(1, 3, 6).unwrap_err();
        assert!(err.contains("min_vcpus"));
    }

    #[test]
    fn test_local_share_borrowers_sorted_by_smallest_deficit_first() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        s.admit_vm(2, 1, 2);
        s.update_vm_metrics(1, 95.0, 0.0);
        s.update_vm_metrics(2, 90.0, 20.0);

        let borrowers = s.local_share_borrowers();
        assert_eq!(borrowers, vec![2, 1]);
    }

    #[test]
    fn test_local_share_donors_are_stable_low_load_vms() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        s.admit_vm(2, 1, 4);
        let _ = s.try_hotplug(1);
        let _ = s.try_hotplug(1);
        let _ = s.try_hotplug(2);
        {
            let mut vms = s.vms.write().unwrap();
            let vm1 = vms.get_mut(&1).unwrap();
            vm1.cpu_usage_pct = DOWNSCALE_TRIGGER_PCT - 5.0;
            vm1.low_load_since = Some(now_secs().saturating_sub(DOWNSCALE_STABLE_SECS + 10));
            let vm2 = vms.get_mut(&2).unwrap();
            vm2.cpu_usage_pct = HOTPLUG_TRIGGER_PCT + 5.0;
        }

        assert_eq!(s.local_share_donors(), vec![1]);
    }

    #[test]
    fn test_local_share_donor_excluded_when_showing_pressure() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        let _ = s.try_hotplug(1);
        {
            let mut vms = s.vms.write().unwrap();
            let vm = vms.get_mut(&1).unwrap();
            vm.cpu_usage_pct = DOWNSCALE_TRIGGER_PCT - 5.0;
            vm.low_load_since = Some(now_secs().saturating_sub(DOWNSCALE_STABLE_SECS + 10));
            vm.steal_pct = DONOR_SAFE_STEAL_PCT + 1.0;
        }

        assert!(s.local_share_donors().is_empty());
    }

    #[test]
    fn test_safe_vcpu_floor_can_stay_above_min_when_usage_requires_it() {
        let s = make_scheduler(4);
        s.admit_vm(1, 1, 4);
        let _ = s.try_hotplug(1);
        let _ = s.try_hotplug(1);
        {
            let mut vms = s.vms.write().unwrap();
            let vm = vms.get_mut(&1).unwrap();
            vm.cpu_usage_pct = 55.0;
            vm.low_load_since = Some(now_secs().saturating_sub(DOWNSCALE_STABLE_SECS + 10));
        }

        let state = s.get_vm_state(1).unwrap();
        assert_eq!(state.safe_vcpu_floor(), 2);
    }

    #[test]
    fn test_set_cpu_weight_marks_local_share_mode() {
        let s = make_scheduler(2);
        s.admit_vm(1, 1, 2);
        assert!(s.set_cpu_weight(1, BOOSTED_CPU_WEIGHT, true));
        let state = s.get_vm_state(1).unwrap();
        assert_eq!(state.cpu_weight, BOOSTED_CPU_WEIGHT);
        assert!(state.local_share_active);
    }

    #[test]
    fn test_steal_pressure_detected() {
        let s = make_scheduler(2);
        s.admit_vm(1, 2, 4);
        s.update_vm_metrics(1, 50.0, 15.0); // steal 15% > STEAL_THRESHOLD_PCT

        let candidates = s.vms_needing_migration();
        assert!(candidates.iter().any(|(id, _)| *id == 1));
    }

    #[test]
    fn test_occupancy_ratio_zero_when_empty() {
        let s = make_scheduler(4);
        assert_eq!(s.occupancy_ratio(), 0.0);
    }

    #[test]
    fn test_occupancy_ratio_increases_with_vms() {
        // occupancy_ratio = slots_saturés / total_slots.
        // Pour 50% avec 12 slots : saturer 6 slots = 6 × 3 VMs = 18 VMs (min=1).
        let s = make_scheduler(4); // 12 slots
        for i in 1u32..=18 {
            s.admit_vm(i, 1, 2); // les 18 premières VMs remplissent 6 slots
        }
        let ratio = s.occupancy_ratio();
        assert!((ratio - 0.5).abs() < 0.01);
    }
}
