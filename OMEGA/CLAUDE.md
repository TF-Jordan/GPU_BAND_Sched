# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**omega-remote-paging** — système de mutualisation de RAM entre nœuds d'un cluster Proxmox VE. Permet de déplacer les pages mémoire froides d'une VM vers la RAM d'un autre nœud, en espace utilisateur via `userfaultfd` (sans modifier le kernel).

Fonctionnalités principales : paging distant, quotas RAM stricts, migration automatique live/cold, scheduler vCPU élastique (hotplug), multiplexeur GPU, scheduler disque via cgroups v2.

## Build & Test Commands

### Rust (workspace)

```bash
# Build release complet (inclut omega-uffd-bridge.so)
make build

# Build debug (plus rapide)
make build-debug
# ou directement :
cargo build --workspace

# Tests unitaires Rust
make test-rust
# ou :
cargo test --workspace

# Test unique
cargo test -p omega-daemon test_parse_vm_cpu_profile

# Lint
make clippy        # cargo clippy --workspace -- -D warnings
make fmt           # cargo fmt --all

# Paquet Debian
make deb
```

### Python (controller)

```bash
# Tests Python
make test-python
# ou :
cd controller && python3 -m pytest tests/ -v

# Test unique
cd controller && python3 -m pytest tests/test_admission.py -v

# Format
make fmt-python    # black controller/ tests/

# Lancer le controller
cd controller && python3 -m controller.main monitor --interval 10
cd controller && python3 -m controller.main status
cd controller && python3 -m controller.main policy --dry-run
```

### Lab interactif (déploiement cluster)

```bash
bash scripts/omega-lab.sh          # menu interactif complet
bash scripts/omega-lab.sh --auto   # CI non-interactif
```

## Architecture

```
Workspace Rust
├── node-a-agent/          # Agent userfaultfd par nœud (compute)
├── node-bc-store/         # Store TCP distant (pages reçues des autres nœuds)
├── omega-daemon/          # Daemon unifié cluster (RAM+CPU+GPU+disque) — port 9100/9200/9300
├── omega-gpu-proxy/       # Proxy GPU applicatif VM → nœud GPU
└── omega-qemu-launcher/   # Wrapper QEMU : injecte memory-backend-file au démarrage

controller/                # Python — orchestre le cluster
├── admission.py           # Validation placement RAM nouvelles VMs
├── cpu_admission.py       # Validation vCPU cluster
├── gpu_admission.py       # Validation VRAM cluster
├── migration_policy.py    # Décision live/cold migration (RAM+CPU+GPU+disque)
├── migration_daemon.py    # Boucle de migration automatique via API Proxmox
├── topology_placement.py  # Score : RAM 50%, topologie 25%, CPU 15%, migrations 10%
├── resilient_collector.py # Collecte état nœuds avec retry + circuit-breaker (cache 120s)
├── proxmox.py             # Client REST Proxmox
└── main.py                # CLI click : status / monitor / policy
```

### Ports réseau

| Port | Protocole | Rôle |
|------|-----------|------|
| 9100 | TCP + TLS | Store inter-nœuds (protocole binaire 20 B header + payload) |
| 9200 | HTTP JSON | API cluster — état nœud pour le controller |
| 9300 | HTTP JSON | Canal de contrôle local — quotas, hotplug, métriques Prometheus |

### Protocole TCP binaire (port 9100)

Trame : `magic 2B | opcode 1B | flags 1B | vm_id 4B | page_id 8B | payload_len 4B | payload`

Opcodes : `PUT_PAGE`, `GET_PAGE`, `DELETE_PAGE`, `PING/PONG`, `OK`, `NOT_FOUND`, `ERROR`, `STATS_REQUEST/RESPONSE`.

### Déploiement selon contexte

| Contexte | Composants |
|----------|------------|
| Cluster Proxmox production | `omega-daemon` (unifié TLS + quota + API) + controller Python |
| Lab KVM / CI | `node-a-agent` + `node-bc-store` — sans Proxmox |
| Nœud standalone | `node-a-agent` + `node-bc-store` local |

## Points clés du code

- **`omega-daemon/src/main.rs`** : point d'entrée du daemon ; lance 8 tâches Tokio (store TCP, API HTTP ×2, VM monitor, CPU monitor 1ms, eviction CLOCK, TLS init, balloon monitor, stats périodiques).
- **`omega-daemon/src/node_state.rs`** : état partagé entre toutes les tâches via `Arc<NodeState>` — contient le quota registry, vcpu_scheduler, disk_io_scheduler, gpu_runtime.
- **`omega-daemon/src/eviction_engine.rs`** : algo CLOCK, lit `/proc/meminfo`, envoie pages froides via TCP. Adapte son agressivité via `FaultBus` (fautes userfaultfd en temps réel).
- **`omega-daemon/src/vcpu_scheduler.rs`** : allocation élastique vCPU — hotplug/unplug via QMP, partage local via `cpu.weight` cgroup, migration si saturation persistante.
- **TLS TOFU** : certificat auto-signé généré dans `/etc/omega-store/tls`, distribution de l'empreinte aux pairs au démarrage.
- **GPU** : source de vérité = backend DRM réel (`/dev/dri/renderD*` + sysfs VRAM) + config Proxmox (`description`/`tags`). Le controller consomme ce que le daemon publie.
- **cgroups v2** : CPU via `cpu.stat`/`cpu.max`/`cpu.weight`, I/O via `io.stat`/`io.weight`, pression via PSI (`io.pressure`).

## Prérequis

- Rust 1.75+
- Python 3.10+
- `rsync` (déploiement SSH)
- SSH root sans mot de passe vers tous les nœuds Proxmox
- Proxmox VE 8.x ou 9.x, Ceph RBD pour le stockage disque VMs

## Logging

Contrôlé par `RUST_LOG` (ex : `RUST_LOG=debug`) ou `--log-level`. Format configurable via `--log-format json` pour la production.
