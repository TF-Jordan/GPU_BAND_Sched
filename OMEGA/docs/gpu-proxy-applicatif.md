# Proxy GPU applicatif Omega

Le proxy GPU applicatif est la voie où les VMs n'accèdent pas directement au
GPU PCI. Elles envoient des jobs à un service hôte `omega-gpu-proxy`; Omega
applique les budgets, limite l'exécution, puis renvoie le résultat.

Cette voie est différente du passthrough, de vGPU et de virtio-gpu:

- elle ne donne pas `/dev/nvidia*` ou `/dev/dri` directement à la VM;
- elle évite le hotplug PCI GPU et reste compatible avec la migration VM;
- elle exige que les applications invitées appellent l'API Omega GPU;
- elle permet de valider le partage applicatif même sans driver GPU dans la VM.

## Démarrage

Compiler:

```bash
cargo build --release -p omega-gpu-proxy
```

Installer via le déploiement Omega sur les nœuds:

```bash
OMEGA_GPU_PRIMARY_NODE=<node-gpu-ou-ip> \
OMEGA_GPU_PROXY_ENABLED=1 \
OMEGA_GPU_PROXY_LISTEN=0.0.0.0:9400 \
OMEGA_GPU_PROXY_MAX_CONCURRENT=1 \
OMEGA_GPU_PROXY_BACKEND_TIMEOUT_SECS=900 \
./scripts/deploy.sh
```

Le script installe:

- `${INSTALL_DIR}/omega-gpu-proxy`;
- `/opt/omega-remote-paging/workers/omega-gpu-worker-app.py`;
- `/opt/omega-remote-paging/workers/omega-gpu-worker-cpu.py`;
- `omega-gpu-proxy.service`.

La configuration persistante est dans `/etc/omega/cluster.env`:

```bash
OMEGA_GPU_PRIMARY_NODE=<node-gpu-ou-ip>
OMEGA_GPU_PROXY_URL=http://<node-gpu-ou-ip>:9400
OMEGA_GPU_PROXY_LISTEN=0.0.0.0:9400
OMEGA_GPU_PROXY_API_TOKEN_FILE=/etc/omega/gpu-proxy.token
OMEGA_GPU_MIGRATE_TO_GPU_NODE=1
OMEGA_GPU_FALLBACK_NETWORK=1
```

`deploy.sh` choisit automatiquement le premier nœud qui répond à
`nvidia-smi -L` comme nœud GPU principal. Tu peux forcer le choix avec
`OMEGA_GPU_PRIMARY_NODE=<node-or-ip>`.

Lancer sur un nœud GPU:

```bash
./target/release/omega-gpu-proxy \
  --node-id <node-gpu> \
  --listen 0.0.0.0:9400 \
  --max-concurrent-jobs 1
```

Si `nvidia-smi` est disponible, le proxy publie le nom du GPU et la VRAM totale.
Sinon il démarre quand même avec un backend de référence CPU. Ce backend sert à
valider l'API, les budgets et la file d'attente; il ne prouve pas une exécution
CUDA réelle.

## Backend externe

Pour brancher un vrai moteur GPU sans changer l'API VM, le proxy peut lancer un
worker externe:

```bash
./target/release/omega-gpu-proxy \
  --node-id <node-gpu> \
  --listen 0.0.0.0:9400 \
  --max-concurrent-jobs 1 \
  --backend-command ./scripts/omega-gpu-worker-cpu.py \
  --backend-timeout-secs 300
```

Contrat du worker:

- `stdin`: un JSON contenant `job_id`, `vm_id`, `kind`, `priority`, `vram_mib`
  et `payload`;
- `stdout`: un JSON résultat;
- code de sortie non nul: le job est marqué `failed`.

Le fichier `scripts/omega-gpu-worker-cpu.py` est un worker de référence. Il est
CPU volontairement, mais il expose le bon contrat pour remplacer le calcul par
PyTorch, ONNX Runtime, CUDA Python ou un binaire métier.

Le fichier `scripts/omega-gpu-worker-app.py` est le worker applicatif complet.
Il supporte plusieurs familles de jobs:

- `matrix_multiply`: PyTorch CUDA si disponible, sinon CPU;
- `inference`: ONNX Runtime si `payload.model_path` est fourni, sinon test
  synthétique PyTorch;
- `video_encode`: `ffmpeg` avec `h264_nvenc` ou `hevc_nvenc`;
- `render`: Blender batch avec Cycles GPU;
- `custom`: commande explicite, désactivée par défaut sauf si
  `OMEGA_GPU_WORKER_ALLOW_CUSTOM=1`.

Lancement recommandé pour valider les vrais backends:

```bash
./target/release/omega-gpu-proxy \
  --node-id <node-gpu> \
  --listen 0.0.0.0:9400 \
  --max-concurrent-jobs 1 \
  --backend-command ./scripts/omega-gpu-worker-app.py \
  --backend-timeout-secs 900
```

Prérequis par backend:

- PyTorch/CUDA: `nvidia-smi` OK, driver NVIDIA chargé, `python3 -c 'import torch; print(torch.cuda.is_available())'` retourne `True`;
- ONNX: `onnxruntime-gpu` et `numpy` installés côté hôte GPU;
- NVENC: `ffmpeg -encoders | grep nvenc` retourne au moins `h264_nvenc`;
- Blender: `blender -b` disponible, Cycles configurable avec CUDA/OptiX.

Exemple de payload reçu par le worker:

```json
{
  "job_id": "uuid",
  "vm_id": 9001,
  "kind": "matrix_multiply",
  "priority": 0,
  "vram_mib": 64,
  "payload": {
    "n": 64,
    "seed": 9001
  }
}
```

## API

Sauf `/health`, l'API peut être protégée par token:

```bash
TOKEN="$(cat /etc/omega/gpu-proxy.token)"
curl -H "Authorization: Bearer ${TOKEN}" http://NODE:9400/gpu/status
```

Le header alternatif `X-Omega-GPU-Token: <token>` est aussi accepté. Si aucun
token n'est configuré, le proxy reste en mode HTTP interne non authentifié, ce
qui est utile en lab mais déconseillé en cluster partagé.

Statut:

```bash
curl -s -H "Authorization: Bearer ${TOKEN}" http://NODE:9400/gpu/status | python3 -m json.tool
```

Déclarer le budget logique d'une VM:

```bash
curl -s -X POST http://NODE:9400/v1/vm/VMID/budget \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"vram_budget_mib":128}' | python3 -m json.tool
```

Soumettre un job applicatif:

```bash
curl -s -X POST http://NODE:9400/v1/jobs \
  -H "Authorization: Bearer ${TOKEN}" \
  -H "Content-Type: application/json" \
  -d '{"vm_id":9001,"kind":"matrix_multiply","vram_mib":64,"payload":{"n":64,"seed":9001}}' \
  | python3 -m json.tool
```

Lire le résultat:

```bash
curl -s -H "Authorization: Bearer ${TOKEN}" http://NODE:9400/v1/jobs/JOB_ID | python3 -m json.tool
```

Client fourni:

```bash
scripts/omega-gpu-client.sh status --proxy http://NODE:9400 --token "$TOKEN"
scripts/omega-gpu-client.sh budget --proxy http://NODE:9400 --token "$TOKEN" --vmid VMID --vram-mib 128
scripts/omega-gpu-client.sh matmul --proxy http://NODE:9400 --token "$TOKEN" --vmid VMID --n 64 --vram-mib 64
scripts/omega-gpu-client.sh inference --proxy http://NODE:9400 --vmid VMID --model-path /models/model.onnx --vram-mib 512
scripts/omega-gpu-client.sh encode --proxy http://NODE:9400 --vmid VMID --codec h264_nvenc --vram-mib 512
scripts/omega-gpu-client.sh render --proxy http://NODE:9400 --vmid VMID --scene-path /data/scene.blend --vram-mib 1024
```

## Test

```bash
OMEGA_GPU_PROXY_URL=http://NODE:9400 \
OMEGA_GPU_PROXY_API_TOKEN="$(cat /etc/omega/gpu-proxy.token)" \
./scripts/tests/32-gpu-proxy.sh VMID
```

Test multi-VM:

```bash
OMEGA_TEST_VMIDS=VMID1,VMID2,VMID3 \
OMEGA_GPU_PROXY_URL=http://NODE:9400 \
OMEGA_GPU_PROXY_API_TOKEN="$(cat /etc/omega/gpu-proxy.token)" \
OMEGA_GPU_PROXY_TEST_VM_COUNT=3 \
./scripts/tests/32-gpu-proxy.sh VMID1
```

Avec le runner:

```bash
OMEGA_GPU_PROXY_URL=http://NODE:9400 ./scripts/tests/run-cluster.sh VMID --gpu
```

Variables utiles:

- `OMEGA_GPU_PROXY_MAX_CONCURRENT`: nombre de jobs applicatifs exécutés en parallèle;
- `OMEGA_GPU_PROXY_TEST_BUDGET_MIB`: budget VRAM logique par VM dans le test;
- `OMEGA_GPU_PROXY_TEST_JOB_VRAM_MIB`: VRAM déclarée par job de test;
- `OMEGA_GPU_PROXY_TEST_VM_COUNT`: nombre de VMs utilisées dans le test multi-VM.
- `OMEGA_GPU_PROXY_API_TOKEN`: token envoyé au proxy pendant les tests.

Si le proxy n'est pas déjà démarré et que `omega-gpu-proxy` a été synchronisé
dans `OMEGA_BIN_DIR`, le test 32 démarre un proxy local sur `127.0.0.1:9400`.

## Ce que la v1 valide

- la VM ou l'hôte peut appeler une API GPU Omega;
- un budget VRAM logique est appliqué par VM;
- un job qui dépasse le budget est refusé;
- les jobs passent par une file d'attente avec concurrence limitée;
- les métriques Prometheus du proxy sont exposées;
- deux VMs peuvent partager le même service GPU par soumission de jobs.
- le contrôleur global peut choisir `local_gpu`, `migrate_to_gpu` ou
  `remote_proxy` selon l'état réel du cluster.

## Contrôleur GPU global

Le contrôleur ne se limite plus à “un proxy sur un nœud”. Pour une VM qui a un
budget `omega_gpu_vram_mib`:

1. Si la VM est déjà sur un nœud GPU avec assez de VRAM libre, Omega garde la VM
   locale et configure le budget GPU.
2. Si la VM est sur un nœud sans GPU, ou si le GPU local est saturé, Omega
   choisit le meilleur nœud GPU capable d'héberger la VM et demande une
   migration via `/control/migrate`.
3. Si aucun nœud GPU ne peut accueillir la VM mais qu'un proxy GPU est disponible,
   Omega configure le budget sur le proxy distant. La VM continue alors par
   appels applicatifs HTTP.
4. Si ni migration ni proxy réseau ne sont possibles, la demande GPU est rejetée
   et journalisée.

Variables:

- `OMEGA_GPU_MIGRATE_TO_GPU_NODE=1`: autorise la migration proactive vers un
  nœud GPU.
- `OMEGA_GPU_FALLBACK_NETWORK=1`: autorise le fallback proxy réseau si la
  migration est impossible.
- `OMEGA_GPU_PRIMARY_NODE`: nœud GPU préféré pour l'API/proxy.
- `OMEGA_GPU_PROXY_URL`: URL du proxy à utiliser par défaut.

La logique de rangement/anti-fragmentation utilise déjà les capacités RAM, vCPU
et GPU dans le contrôleur: elle préfère les nœuds capables d'accueillir la VM
sans casser les budgets, et évite les migrations répétées via cooldown.

## Packaging release

Construire un paquet Debian:

```bash
make deb
ls -lh target/deb/omega-remote-paging_*_amd64.deb
```

Installer sur un nœud:

```bash
dpkg -i target/deb/omega-remote-paging_*_amd64.deb
omega-node-install
```

Le paquet installe les binaires, scripts, workers et docs sous
`/opt/omega-remote-paging`. L'activation Proxmox destructive du wrapper QEMU
reste explicite via `omega-node-install`.

## Ce que la v1 ne valide pas encore

- interception transparente des appels CUDA dans la VM;
- isolation mémoire GPU garantie par driver;
- priorité préemptive d'un kernel GPU déjà lancé.

Pour valider une exécution CUDA/PyTorch réelle, il faut fournir un
`--backend-command` qui appelle réellement le GPU côté hôte et qui respecte le
contrat JSON ci-dessus.
