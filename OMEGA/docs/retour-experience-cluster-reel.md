# Retour d'expérience — cluster réel, installation, bugs et corrections

Ce document rassemble le parcours réel de mise en service du projet sur un cluster Proxmox physique, les erreurs rencontrées, les corrections appliquées dans le code, les contournements provisoires et l'état actuel.

Il sert à éviter de refaire les mêmes diagnostics plus tard.

---

## 1. Contexte réel

Cluster utilisé :

| Nœud | Rôle observé |
|------|--------------|
| `pve` | controller + daemon + nœud Proxmox |
| `pve2` | daemon + nœud Proxmox |
| `pve3` | daemon + nœud Proxmox |

Points structurants du déploiement :

- `omega-daemon` doit tourner sur chaque nœud Proxmox.
- `omega-controller` doit tourner sur une seule machine ou un seul nœud.
- le réseau interne du cluster peut rester isolé, avec accès Internet sortant via NAT si nécessaire.
- les disques VM sont idéalement placés sur Ceph RBD pour faciliter les migrations.

---

## 2. Réseau et accès Internet

### Situation de départ

Le cluster utilisait un réseau privé `10.10.0.0/24` derrière un bridge local, sans sortie Internet directe.

### Solution retenue

Le bridge privé du cluster a été conservé, et l'accès Internet a été fourni via NAT sortant vers l'interface qui portait déjà la route par défaut du nœud hôte.

Constat important :

- dans le cas observé, la sortie principale passait par `eno2`, pas `wlo1`
- le bridge de cluster devait rester un réseau isolé, mais pas forcément un réseau sans sortie
- le bon modèle est : réseau privé cluster + NAT sortant, pas bridge direct sur le Wi-Fi

### À retenir

- l'isolement du cluster n'implique pas l'absence d'accès Internet sortant
- pour les dépendances (`apt`, `git`, `pip`), le NAT est suffisant
- il faut garder séparés les usages cluster/stockage/migration et l'accès Internet

---

## 3. Controller Proxmox API

### Découverte importante

Le token API Proxmox ne doit être créé qu'une seule fois au niveau cluster, pas sur chaque nœud.

Commande utilisée :

```bash
pveum user token add root@pam omega-controller --privsep 0
```

Le token doit ensuite être placé uniquement sur la machine qui héberge `omega-controller`.

### Format attendu

```bash
OMEGA_PROXMOX_TOKEN=root@pam!omega-controller=SECRET
```

`OMEGA_PROXMOX_TOKEN` ne doit pas contenir `PVEAPIToken=` ; c'est le code qui s'en charge.

### Architecture confirmée

- `omega-daemon` : partout
- `omega-controller` : une seule instance

---

## 4. Installation Python du controller

### Erreur rencontrée

L'installation editable du package controller échouait avec :

```text
BackendUnavailable: Cannot import 'setuptools.backends.legacy'
```

### Cause

Le `build-backend` de `controller/pyproject.toml` était incorrect.

### Correction appliquée

Le backend a été corrigé vers :

```toml
[build-system]
requires      = ["setuptools>=68", "wheel"]
build-backend = "setuptools.build_meta"
```

### Effet

`pip install -e /opt/omega-remote-paging/controller/` fonctionne ensuite normalement.

---

## 5. Intégration Proxmox réelle dans le controller

### Bug rencontré

Le controller appelait l'API Proxmox avec des identifiants internes `node-a`, `node-b`, `node-c`, alors que Proxmox attendait les vrais noms de nœuds (`pve`, `pve2`, `pve3`).

Erreur observée :

```text
hostname lookup 'node-a' failed
```

### Correction

La réconciliation controller utilise maintenant le vrai nom de nœud Proxmox remonté par le daemon.

### Effet

Le controller peut lire la configuration des VMs et déclencher des migrations avec les bons identifiants Proxmox.

---

## 6. Boucles de repositionnement automatique

### Symptôme

Le controller recalculait en boucle qu'une VM devait aller sur un autre nœud et relançait la même migration à chaque poll.

### Cause

Le controller ne mémorisait pas correctement qu'une tentative de repositionnement était déjà en cours.

### Correction

Ajout d'un cooldown spécifique aux migrations automatiques d'admission.

### Effet

Les logs deviennent du type :

- `repositionnement automatique accepté`
- `repositionnement automatique déjà en attente`

au lieu de spawner la même tentative toutes les 5 secondes.

---

## 7. Choix live/cold incorrect pour des VMs actives

### Symptôme

Le daemon recevait parfois une migration `cold` pour une VM qui tournait encore.

Erreur observée :

```text
can't migrate running VM without --online
```

### Cause

Le controller utilisait `cold` dès que l'état n'était pas exactement `running`, alors que des VMs locales pouvaient apparaître comme `unknown`.

### Correction

Le type automatique est maintenant :

- `cold` seulement si l'état est explicitement `stopped`
- `live` sinon

### Effet

Les migrations live réelles fonctionnent correctement avec `qm migrate ... --online`.

---

## 8. VMs arrêtées encore visibles dans le scheduler CPU

### Symptôme

Une VM arrêtée disparaissait de l'usage CPU, mais restait visible dans `vm_states` côté `/control/vcpu/status`.

### Cause

Le scheduler CPU utilisait un snapshot trop large du tracker local et gardait aussi les VMs `stopped`.

### Correction

Le daemon ne suit maintenant dans le scheduler CPU que les VMs locales en cours d'exécution.

### Effet

Une VM arrêtée est purgée du scheduler au cycle suivant.

---

## 9. Élasticité CPU — première série de bugs

### 9.1 Erreur de type CPU QMP

#### Symptôme

Le scheduler décidait de monter à `2`, mais la VM restait à `1`.

Logs observés :

```text
device_add cpu échoué
Invalid CPU type, expected cpu type: 'kvm64-x86_64-cpu'
```

#### Cause

Le hotplug utilisait un type CPU codé en dur incompatible avec la VM réelle.

#### Correction

Le daemon interroge maintenant `query-hotpluggable-cpus` et utilise :

- le vrai `type`
- le vrai `socket-id`
- le vrai `core-id`
- le vrai `thread-id`

#### Effet

Le hotplug utilise le bon type QMP par VM.

### 9.2 Chemin cgroup CPU non détecté

#### Symptôme

Dans le guest, la charge CPU était réelle, mais `cpu_usage_pct` restait à `0.0` côté daemon.

#### Cause

Sur le nœud réel, la VM était dans :

```text
/sys/fs/cgroup/qemu.slice/<vmid>.scope
```

alors que le code cherchait surtout sous `machine.slice/...`.

#### Correction

Le contrôleur CPU cgroup prend maintenant en charge `qemu.slice/<vmid>.scope` en plus des autres formes Proxmox.

#### Effet

`cpu_usage_pct` reflète enfin la charge réelle du guest.

### 9.3 Mauvaise capacité CPU QMP

#### Symptôme

Malgré `-smp '1,sockets=1,cores=4,maxcpus=4'`, le daemon enregistrait encore la VM avec `max_vcpus=1`.

#### Causes combinées

1. `query-cpus-fast` pouvait ne pas exposer le champ `online`
2. l'enregistrement initial du scheduler dépendait trop de QMP

#### Corrections

- la lecture QMP interprète correctement les CPU déjà présents
- la capacité totale hotpluggable est dérivée de `query-hotpluggable-cpus`
- surtout, le scheduler est maintenant initialisé à partir de la config Proxmox locale (`/etc/pve/qemu-server/<vmid>.conf`) :
  - `min_vcpus = vcpus`
  - `max_vcpus = sockets × cores`

#### Effet

Le scheduler connaît dès le départ le vrai plafond CPU de la VM.

---

## 10. Politique CPU réelle retenue

La politique finale retenue pour le CPU est :

- une VM démarre à son `min_vcpus`
- elle ne reçoit pas tout son plafond au boot
- sous charge, le daemon monte progressivement jusqu'à `max_vcpus`
- à faible charge stable, il redescend progressivement
- si une VM est sous pression mais qu'aucun slot libre n'existe, le daemon essaie d'abord de récupérer 1 vCPU réel sur une VM durablement idle
  à condition que la donneuse reste à un niveau égal ou supérieur à un plancher
  de sécurité dérivé de `min_vcpus` et de son utilisation récente, sans throttle
  ni pression CPU
- si aucun retrait réel n'est possible, le daemon active un partage CPU local temporaire via `cpu.weight`
- si le nœud manque de CPU, la VM reste vivante
- si un autre nœud améliore la situation globale du cluster, le controller la migre
- si aucun nœud ne peut la satisfaire complètement, elle reste vivante avec un déficit temporaire

Le projet utilise donc :

- un pool logique cluster-wide
- des hotplugs locaux côté daemon
- une réclamation locale des VMs idle avant migration
- un partage CPU local temporaire par priorité cgroup
- des migrations best-effort côté controller

Pas de vrai "remote CPU execution" inter-nœuds.

---

## 11. GPU — stratégie retenue

Le cluster réel ne dispose pas du matériel/licensing NVIDIA pour `vGPU` ou `MIG`.

La stratégie retenue dans le projet reste donc :

- partage logiciel existant
- budget GPU par VM
- placement et migration vers le nœud GPU le moins chargé
- évacuation préalable d'une autre VM GPU si cela crée l'espace nécessaire

Ce n'est pas équivalent à un vrai `vGPU` natif Proxmox, mais c'est cohérent avec le matériel réellement disponible.

---

## 12. Maintenance et extinction d'un nœud

### Constat réel

Si on éteint un nœud directement :

- les VMs qui tournent dessus s'arrêtent avec lui
- c'est normal
- elles ne migrent pas toutes seules juste parce que le nœud s'arrête

### Ce qui existait avant

Il existait seulement :

- des migrations opportunistes
- une raison `maintenance`
- pas de vrai flux `drain node`

### Ce qui a été ajouté

Une commande controller de drain manuel a été ajoutée :

```bash
omega-controller drain-node \
  --node-a http://10.10.0.11:9300 \
  --node-b http://10.10.0.12:9300 \
  --node-c http://10.10.0.13:9300 \
  --source-node node-b
```

Elle :

- inspecte l'état cluster
- planifie l'évacuation des VMs du nœud source
- lance les migrations avec `reason=maintenance`
- attend jusqu'à ce que le nœud soit vide

### Limites actuelles

- ce n'est pas encore un `cordon` persistant
- il faut lancer explicitement `drain-node`
- ensuite seulement éteindre le nœud

---

## 13. Limites connues restantes

### 13.1 Migration `target=auto` côté daemon

Des logs réels ont montré :

```text
qm migrate 9004 auto --online
no such cluster node 'auto'
```

Conclusion :

- le daemon seul ne sait pas résoudre un vrai nœud cible à partir de `auto`
- la vue cluster et la résolution de cible doivent venir du controller

Tant que ce point n'est pas réécrit, il faut considérer `target=auto` comme non fiable côté daemon autonome.

### 13.2 Drain manuel seulement

Le drain existe maintenant côté controller, mais il manque encore :

- un état persistant `maintenance`
- un `cordon/uncordon`
- un blocage durable des nouvelles admissions sur un nœud drainé

### 13.3 Déploiement multi-nœuds

Le cluster a montré plusieurs fois qu'un nœud pouvait garder un ancien binaire pendant que les autres étaient déjà corrigés.

Règle pratique :

- dès qu'un bug daemon est corrigé, déployer le même binaire sur les trois nœuds
- dès qu'un bug controller est corrigé, redéployer le controller unique

---

## 14. Procédure de maintenance recommandée

Pour arrêter proprement un nœud désormais :

1. lancer `omega-controller drain-node --source-node <node>`
2. attendre que le nœud soit vide
3. vérifier qu'aucune VM ne tourne encore dessus
4. seulement ensuite faire `systemctl poweroff`

Pour réintégrer un nœud :

1. rallumer le nœud
2. vérifier `omega-daemon`
3. vérifier `/api/status` et `/control/status`
4. remettre le scheduler/controller en fonctionnement normal

---

## 15. Résumé très court

Ce qui a été validé sur le cluster réel :

- token Proxmox API et controller unique
- migrations live réelles via Proxmox
- purge des VMs stoppées du scheduler
- lecture CPU réelle via cgroups Proxmox
- hotplug QMP avec bon type CPU
- prise en compte du vrai plafond CPU de la VM
- drain manuel de maintenance côté controller

Ce qui reste à faire si on veut aller plus loin :

- `cordon/uncordon` persistant
- HA Proxmox si on veut une reprise après panne sans drain manuel préalable
