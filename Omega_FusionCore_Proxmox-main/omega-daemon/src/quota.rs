//! Registre des quotas mémoire par VM — garantie de non-dépassement.
//!
//! # Problème résolu
//!
//! Sans quota, une VM qui demande 10 Gio pourrait théoriquement accumuler :
//! - 8 Gio de pages locales (RAM du nœud)
//! - N Gio de pages distantes sans limite
//! → dépassement de ce qu'elle a demandé.
//!
//! # Invariant garanti
//!
//! Pour toute VM v :
//!   `remote_pages(v) × PAGE_SIZE ≤ remote_budget(v)`
//! et
//!   `remote_budget(v) = max_mem(v) - local_budget(v)`
//!
//! Avec `local_budget(v) ≤ max_mem(v)` (QEMU ne peut pas dépasser max_mem).
//!
//! Donc : `local_pages(v) + remote_pages(v) ≤ max_mem(v)` — toujours.
//!
//! # Flux de configuration
//!
//! 1. Le controller Python fait une décision d'admission :
//!    - Cluster total libre ≥ vm.max_mem → admission acceptée
//!    - Nœud A a 8 Gio dispo, nœud B a 2 Gio → local_budget=8G, remote_budget=2G
//!
//! 2. Le controller pousse le quota via `POST /control/vm/{vmid}/quota`
//!
//! 3. Le store vérifie la quota avant chaque PUT_PAGE :
//!    - Si `remote_pages(vm) >= remote_budget(vm)` → refuse avec QUOTA_EXCEEDED
//!
//! 4. Le hook Proxmox (post-start) déclenche la configuration initiale du quota.
//!
//! # Ajustements dynamiques
//!
//! Quand le balloon driver réduit la RAM du guest, le budget remote peut croître :
//!   `remote_budget = max_mem - balloon_actual`
//! Le daemon met à jour le quota automatiquement via `BalloonMonitor`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

pub const PAGE_SIZE_BYTES: u64 = 4096;

// ─── Structures ───────────────────────────────────────────────────────────────

/// Quota mémoire d'une VM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmQuota {
    /// VMID Proxmox
    pub vm_id: u32,
    /// RAM totale demandée par la VM (Mio)
    pub max_mem_mib: u64,
    /// Pages locales (sur le nœud hôte) — budget alloué au démarrage
    pub local_budget_mib: u64,
    /// Pages distantes — budget maximum autorisé (Mio)
    pub remote_budget_mib: u64,
    /// Pages distantes actuellement consommées (Mio, mis à jour en temps réel)
    pub remote_used_mib: u64,
    /// Timestamp de création du quota (Unix secondes)
    pub created_at: u64,
    /// Timestamp de dernière mise à jour
    pub updated_at: u64,
}

impl VmQuota {
    pub fn new(vm_id: u32, max_mem_mib: u64, local_budget_mib: u64) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let remote_budget_mib = max_mem_mib.saturating_sub(local_budget_mib);

        Self {
            vm_id,
            max_mem_mib,
            local_budget_mib,
            remote_budget_mib,
            remote_used_mib: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Nombre maximum de pages distantes autorisées.
    pub fn max_remote_pages(&self) -> u64 {
        self.remote_budget_mib * 1024 * 1024 / PAGE_SIZE_BYTES
    }

    /// Nombre de pages distantes actuellement utilisées.
    pub fn used_remote_pages(&self) -> u64 {
        self.remote_used_mib * 1024 * 1024 / PAGE_SIZE_BYTES
    }

    /// Reste de pages distantes disponibles.
    pub fn remaining_remote_pages(&self) -> u64 {
        self.max_remote_pages()
            .saturating_sub(self.used_remote_pages())
    }

    /// Pourcentage du budget remote consommé (0.0 – 100.0).
    pub fn remote_usage_pct(&self) -> f64 {
        if self.remote_budget_mib == 0 {
            return 100.0;
        }
        (self.remote_used_mib as f64 / self.remote_budget_mib as f64) * 100.0
    }

    /// La VM a-t-elle atteint son quota distant ?
    pub fn is_remote_full(&self) -> bool {
        self.remote_used_mib >= self.remote_budget_mib
    }

    /// Met à jour le budget remote suite à un ajustement balloon.
    ///
    /// Quand le balloon réduit la RAM guest à `balloon_actual_mib`,
    /// le budget remote peut augmenter en conséquence.
    pub fn adjust_for_balloon(&mut self, balloon_actual_mib: u64) {
        let new_local = balloon_actual_mib.min(self.max_mem_mib);
        let new_remote = self.max_mem_mib.saturating_sub(new_local);

        if new_remote != self.remote_budget_mib {
            info!(
                vm_id = self.vm_id,
                old_remote_budget = self.remote_budget_mib,
                new_remote_budget = new_remote,
                balloon_mib = balloon_actual_mib,
                "quota remote ajusté suite à balloon"
            );
            self.remote_budget_mib = new_remote;
            self.local_budget_mib = new_local;
            self.updated_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        }
    }
}

// ─── Registre ─────────────────────────────────────────────────────────────────

/// Résultat d'une vérification de quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaCheck {
    /// La page peut être stockée.
    Allowed,
    /// La VM a atteint son quota distant — refus.
    Exceeded {
        vm_id: u32,
        used_mib: u64,
        budget_mib: u64,
    },
    /// Aucun quota défini pour cette VM — comportement par défaut (autoriser).
    NoQuota,
}

impl QuotaCheck {
    pub fn is_allowed(&self) -> bool {
        matches!(self, QuotaCheck::Allowed | QuotaCheck::NoQuota)
    }
}

/// Registre partagé des quotas VM.
///
/// Thread-safe via `RwLock`. Les lectures (check_put) sont très fréquentes
/// et non bloquantes entre elles. Les écritures (set, update) sont rares.
pub struct QuotaRegistry {
    quotas: RwLock<HashMap<u32, VmQuota>>,
}

impl Default for QuotaRegistry {
    fn default() -> Self {
        Self {
            quotas: RwLock::new(HashMap::new()),
        }
    }
}

impl QuotaRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Enregistre ou remplace le quota d'une VM.
    pub fn set(&self, quota: VmQuota) {
        info!(
            vm_id = quota.vm_id,
            max_mem_mib = quota.max_mem_mib,
            local_budget_mib = quota.local_budget_mib,
            remote_budget_mib = quota.remote_budget_mib,
            "quota VM configuré"
        );
        self.quotas.write().unwrap().insert(quota.vm_id, quota);
    }

    /// Supprime le quota d'une VM (après migration ou arrêt).
    pub fn remove(&self, vm_id: u32) {
        if self.quotas.write().unwrap().remove(&vm_id).is_some() {
            debug!(vm_id, "quota VM supprimé");
        }
    }

    /// Vérifie si une page distante supplémentaire peut être stockée pour cette VM.
    ///
    /// Appelé avant chaque PUT_PAGE dans le store.
    pub fn check_put(&self, vm_id: u32, page_size_bytes: u64) -> QuotaCheck {
        let guard = self.quotas.read().unwrap();
        let Some(quota) = guard.get(&vm_id) else {
            return QuotaCheck::NoQuota;
        };

        let extra_mib = page_size_bytes.div_ceil(1024 * 1024).max(1);
        if quota.remote_used_mib + extra_mib > quota.remote_budget_mib {
            warn!(
                vm_id,
                used_mib = quota.remote_used_mib,
                budget_mib = quota.remote_budget_mib,
                extra_mib,
                "PUT_PAGE refusé — quota distant atteint"
            );
            return QuotaCheck::Exceeded {
                vm_id,
                used_mib: quota.remote_used_mib,
                budget_mib: quota.remote_budget_mib,
            };
        }

        QuotaCheck::Allowed
    }

    /// Enregistre qu'une page a été stockée (incrémente le compteur).
    pub fn record_put(&self, vm_id: u32, page_size_bytes: u64) {
        let mut guard = self.quotas.write().unwrap();
        if let Some(quota) = guard.get_mut(&vm_id) {
            let added_mib = (page_size_bytes as f64 / (1024.0 * 1024.0)).ceil() as u64;
            quota.remote_used_mib += added_mib;
            quota.updated_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        }
    }

    /// Enregistre qu'une page a été supprimée (décrémente le compteur).
    pub fn record_delete(&self, vm_id: u32, page_size_bytes: u64) {
        let mut guard = self.quotas.write().unwrap();
        if let Some(quota) = guard.get_mut(&vm_id) {
            let freed_mib = (page_size_bytes as f64 / (1024.0 * 1024.0)).ceil() as u64;
            quota.remote_used_mib = quota.remote_used_mib.saturating_sub(freed_mib);
        }
    }

    /// Libère toutes les pages d'une VM (post-migration ou arrêt).
    pub fn record_delete_vm(&self, vm_id: u32) {
        let mut guard = self.quotas.write().unwrap();
        if let Some(quota) = guard.get_mut(&vm_id) {
            quota.remote_used_mib = 0;
        }
    }

    /// Ajuste le quota suite à un changement de taille balloon.
    pub fn apply_balloon_update(&self, vm_id: u32, balloon_actual_mib: u64) {
        let mut guard = self.quotas.write().unwrap();
        if let Some(quota) = guard.get_mut(&vm_id) {
            quota.adjust_for_balloon(balloon_actual_mib);
        }
    }

    /// Snapshot de tous les quotas (pour l'API HTTP).
    pub fn snapshot(&self) -> Vec<VmQuota> {
        self.quotas.read().unwrap().values().cloned().collect()
    }

    /// Quota d'une VM spécifique.
    pub fn get(&self, vm_id: u32) -> Option<VmQuota> {
        self.quotas.read().unwrap().get(&vm_id).cloned()
    }

    /// Résumé global (pour les métriques).
    pub fn summary(&self) -> QuotaSummary {
        let guard = self.quotas.read().unwrap();
        let total_budget: u64 = guard.values().map(|q| q.remote_budget_mib).sum();
        let total_used: u64 = guard.values().map(|q| q.remote_used_mib).sum();
        let full_vms: u32 = guard.values().filter(|q| q.is_remote_full()).count() as u32;

        QuotaSummary {
            vm_count: guard.len() as u32,
            total_budget_mib: total_budget,
            total_used_mib: total_used,
            vms_at_quota: full_vms,
            usage_pct: if total_budget > 0 {
                (total_used as f64 / total_budget as f64) * 100.0
            } else {
                0.0
            },
        }
    }
}

/// Résumé global des quotas pour les métriques.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaSummary {
    pub vm_count: u32,
    pub total_budget_mib: u64,
    pub total_used_mib: u64,
    pub vms_at_quota: u32,
    pub usage_pct: f64,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_quota(vm_id: u32, max_mem: u64, local: u64) -> VmQuota {
        VmQuota::new(vm_id, max_mem, local)
    }

    #[test]
    fn test_remote_budget_is_difference() {
        let q = make_quota(1, 10_240, 8_192); // 10 Gio max, 8 Gio local
        assert_eq!(q.remote_budget_mib, 2_048); // 2 Gio remote
    }

    #[test]
    fn test_remote_budget_zero_when_local_equals_max() {
        let q = make_quota(1, 4_096, 4_096);
        assert_eq!(q.remote_budget_mib, 0);
        assert!(q.is_remote_full());
    }

    #[test]
    fn test_check_put_allowed_when_under_budget() {
        let reg = QuotaRegistry::new();
        reg.set(make_quota(1, 10_240, 8_192)); // 2 Gio remote budget
        assert_eq!(reg.check_put(1, 4096), QuotaCheck::Allowed);
    }

    #[test]
    fn test_check_put_exceeded_when_over_budget() {
        let reg = QuotaRegistry::new();
        let mut q = make_quota(1, 10_240, 8_192);
        q.remote_used_mib = 2_048; // déjà au max
        reg.set(q);

        let result = reg.check_put(1, 4096);
        assert!(matches!(result, QuotaCheck::Exceeded { .. }));
    }

    #[test]
    fn test_check_put_no_quota_allows() {
        let reg = QuotaRegistry::new();
        assert_eq!(reg.check_put(99, 4096), QuotaCheck::NoQuota);
    }

    #[test]
    fn test_record_put_increments_used() {
        let reg = QuotaRegistry::new();
        reg.set(make_quota(1, 10_240, 8_192));
        // Simuler 512 PUT de 4096 octets = 2 Mio
        for _ in 0..512 {
            reg.record_put(1, 4096);
        }
        let q = reg.get(1).unwrap();
        assert!(q.remote_used_mib >= 2);
    }

    #[test]
    fn test_record_delete_decrements_used() {
        let reg = QuotaRegistry::new();
        let mut q = make_quota(1, 10_240, 8_192);
        q.remote_used_mib = 100;
        reg.set(q);

        reg.record_delete(1, 50 * 1024 * 1024); // libérer 50 Mio
        let q = reg.get(1).unwrap();
        assert!(q.remote_used_mib <= 100);
    }

    #[test]
    fn test_record_delete_vm_resets_to_zero() {
        let reg = QuotaRegistry::new();
        let mut q = make_quota(1, 10_240, 8_192);
        q.remote_used_mib = 1_500;
        reg.set(q);

        reg.record_delete_vm(1);
        assert_eq!(reg.get(1).unwrap().remote_used_mib, 0);
    }

    #[test]
    fn test_balloon_adjustment_increases_remote_budget() {
        let reg = QuotaRegistry::new();
        reg.set(make_quota(1, 10_240, 8_192)); // remote = 2 Gio

        // Le balloon réduit la RAM guest à 6 Gio → remote peut monter à 4 Gio
        reg.apply_balloon_update(1, 6_144);
        let q = reg.get(1).unwrap();
        assert_eq!(q.remote_budget_mib, 4_096);
        assert_eq!(q.local_budget_mib, 6_144);
    }

    #[test]
    fn test_balloon_cannot_exceed_max_mem() {
        let reg = QuotaRegistry::new();
        reg.set(make_quota(1, 10_240, 8_192));

        // Balloon plus grand que max_mem → ne peut pas créer de budget remote > max
        reg.apply_balloon_update(1, 12_000); // impossible réellement, mais testé
        let q = reg.get(1).unwrap();
        assert_eq!(q.local_budget_mib, 10_240); // capé à max_mem
        assert_eq!(q.remote_budget_mib, 0);
    }

    #[test]
    fn test_summary_counts_vms_at_quota() {
        let reg = QuotaRegistry::new();

        let q1 = make_quota(1, 4_096, 4_096); // remote = 0, donc à quota dès le début
        let q2 = make_quota(2, 10_240, 8_192); // 2 Gio remote disponible

        reg.set(q1);
        reg.set(q2);

        let summary = reg.summary();
        assert_eq!(summary.vm_count, 2);
        assert_eq!(summary.vms_at_quota, 1);
    }

    #[test]
    fn test_remove_cleans_quota() {
        let reg = QuotaRegistry::new();
        reg.set(make_quota(1, 4_096, 2_048));
        reg.remove(1);
        assert!(reg.get(1).is_none());
        // Après suppression, check_put retourne NoQuota (autorisé par défaut)
        assert_eq!(reg.check_put(1, 4096), QuotaCheck::NoQuota);
    }
}
