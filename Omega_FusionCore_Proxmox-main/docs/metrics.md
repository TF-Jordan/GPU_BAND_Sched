# Métriques — omega-remote-paging V1

## node-bc-store

Métriques exposées via l'opcode `STATS_REQUEST` (réponse JSON).

| Métrique               | Type    | Description                                          |
|------------------------|---------|------------------------------------------------------|
| `pages_stored`         | counter | Nombre de pages actuellement en RAM dans le store    |
| `estimated_bytes`      | gauge   | `pages_stored × 4096` — mémoire brute utilisée       |
| `put_count`            | counter | Total de PUT_PAGE reçus                              |
| `get_count`            | counter | Total de GET_PAGE reçus                              |
| `delete_count`         | counter | Total de DELETE_PAGE reçus                           |
| `hit_count`            | counter | GET_PAGE ayant retourné une page (OK)                |
| `miss_count`           | counter | GET_PAGE n'ayant rien trouvé (NOT_FOUND)             |
| `hit_rate_pct`         | gauge   | `hit_count / get_count × 100` — en pourcentage       |
| `active_connections`   | gauge   | Connexions TCP actives en ce moment                  |
| `pages_orphaned_deleted` | counter | Pages supprimées par OrphanCleaner (VMs disparues du cluster) |
| `disk_available_mib`   | gauge   | Espace disque disponible sur le nœud store (via `statvfs`) |
| `ceph_enabled`         | gauge   | 1 si le backend Ceph RADOS est actif, 0 si store RAM  |

### Exemple de réponse STATS

```json
{
  "node_id": "node-b",
  "pages_stored": 1024,
  "estimated_bytes": 4194304
}
```

### Logs périodiques (toutes les N secondes, configurable)

```
[INFO] stats périodiques node_id=node-b pages=1024 bytes=4194304
       puts=2000 gets=1500 hits=1480 misses=20 hit_rate_pct=98.7
       connections=2
```

---

## node-a-agent

Métriques atomiques internes (loggées à la fin du mode demo, ou accessibles via HTTP sur le port 9300).

| Métrique               | Description                                               |
|------------------------|-----------------------------------------------------------|
| `fault_count`          | Nombre total de page faults interceptés par userfaultfd   |
| `fault_served`         | Faults correctement résolus (UFFDIO_COPY réussi)          |
| `fault_errors`         | Faults ayant échoué (page zéro injectée en secours)       |
| `pages_evicted`        | Pages envoyées vers les stores via PUT_PAGE               |
| `pages_fetched`        | Pages récupérées depuis les stores via GET_PAGE            |
| `fetch_zeros`          | Pages retournées comme zéro (page absente du store)       |
| `vcpu_current`         | Nombre de vCPUs actuellement attribués à la VM            |
| `vcpu_utilization_pct` | Charge CPU mesurée via cgroup v2 `usage_usec` (ou fallback `/proc/<pid>/stat`) |
| `cpu_pressure`         | Indicateur booléen de pression CPU (AtomicBool) — déclenche candidature à la migration |

### Santé attendue en fonctionnement normal

```
fault_errors = 0       (toutes les faults sont servies)
fetch_zeros  = 0       (toutes les pages évinvées sont retrouvées)
fault_served = fault_count  (chaque fault est servie)
pages_fetched ≤ pages_evicted  (cohérent si pas de double fetch)
```

---

## Métriques système Linux — /proc/meminfo (relevées par le controller)

| Champ              | Description                                                  |
|--------------------|--------------------------------------------------------------|
| `MemTotal`         | RAM totale physique (Ko)                                     |
| `MemAvailable`     | RAM disponible sans swapper (estimation kernel) (Ko)         |
| `SwapTotal`        | Espace swap total (Ko)                                       |
| `SwapFree`         | Espace swap libre (Ko)                                       |
| `usage_pct`        | `(MemTotal - MemAvailable) / MemTotal × 100`                |
| `swap_usage_pct`   | `(SwapTotal - SwapFree) / SwapTotal × 100`                  |

---

## PSI — Pressure Stall Information (/proc/pressure/memory)

| Champ              | Description                                                  |
|--------------------|--------------------------------------------------------------|
| `some.avg10`       | % du temps (10s) où ≥1 thread attendait de la mémoire       |
| `some.avg60`       | idem sur 60s                                                 |
| `some.avg300`      | idem sur 300s                                                |
| `full.avg10`       | % du temps (10s) où TOUS les threads attendaient de la mémoire|
| `full.avg60`       | idem sur 60s                                                 |
| `full.avg300`      | idem sur 300s                                                |

### Interprétation PSI

| Valeur `some.avg10` | Interprétation                          |
|---------------------|-----------------------------------------|
| 0 – 1%              | Aucune pression                         |
| 1 – 10%             | Pression modérée — surveiller           |
| > 10%               | Pression élevée — activer remote paging |
| > 20%               | Pression critique — envisager migration |

---

## Endpoint Prometheus — /control/metrics (implémenté V4)

Le daemon expose des métriques au format Prometheus sur `GET /control/metrics` (port 9300).

```bash
curl http://192.168.10.1:9300/control/metrics
```

Métriques exposées :

```
omega_vcpu_slots_total{node="pve-node-a"}  36
omega_vcpu_slots_used{node="pve-node-a"}   12
omega_vcpu_slots_free{node="pve-node-a"}   24
omega_vcpu_steal_pct{node="pve-node-a"}    1.20
omega_vcpu_vm_count{node="pve-node-a"}     4
omega_gpu_vram_total_mib{node="pve-node-a"} 8192
omega_gpu_vram_reserved_mib{node="pve-node-a"} 4096
omega_gpu_vram_free_mib{node="pve-node-a"} 4096
```

Ainsi que les métriques store (pages_stored, put_count, get_count, hit_rate_pct, connections)
exposées via `GET /control/status` au format JSON, et les métriques GPU détaillées via
`GET /control/gpu/status`.

## Roadmap métriques

- Grafana dashboard template (scrape `/control/metrics` via Prometheus)
- Alertes configurables via le controller (webhook Slack/PagerDuty)
- Métriques RDMA (latence, débit, erreurs) si passage à RDMA
