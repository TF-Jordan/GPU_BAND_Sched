# Guide des expériences — omega-remote-paging V1

## Prérequis

### Matériel

| Nœud   | Rôle            | RAM min. | Réseau          |
|--------|-----------------|----------|-----------------|
| nœud A | Compute (VMs)   | 8 Gio    | GbE (≥1Gb/s)   |
| nœud B | Memory store 1  | 16 Gio   | GbE             |
| nœud C | Memory store 2  | 16 Gio   | GbE             |

### Logiciel (sur chaque nœud)

```bash
# Kernel ≥ 5.11 recommandé (userfaultfd sans root)
uname -r

# Proxmox VE 8.x ou 9.x
pveversion

# Rust toolchain — compiler sur la machine de dev, déployer le binaire
# (voir docs/developpement-et-deploiement.md)
rustc --version  # ≥ 1.75

# Python 3.11+ (pour le controller)
python3 --version

# Outils de test
apt install stress-ng netcat-openbsd
```

---

## Expérience 1 : Validation locale (1 machine)

Simule les 3 nœuds sur une seule machine. Idéal pour valider le prototype avant déploiement cluster.

### Étapes

```bash
# 1. Compilation
make build

# 2. Test automatisé complet
make test-integration

# Ou manuellement :
./scripts/test_scenario.sh --build
```

### Métriques à observer

Pendant l'exécution du scénario demo, vous devriez voir :
- Les stores logger les PUT_PAGE reçus
- L'agent logger les page faults interceptés
- Le résultat final "SUCCÈS : toutes les pages lues avec intégrité correcte"

---

## Expérience 2 : Déploiement cluster réel (3 nœuds Proxmox)

### Préparation

```bash
# Sur la machine de développement : compiler et déployer
cargo build --release -p omega-daemon
./deploy.sh lab          # ou ./deploy.sh prod
# (voir docs/developpement-et-deploiement.md pour les détails)
```

### Vérification des stores

```bash
# Depuis nœud A : ping des stores
nc -z -w2 192.168.1.2 9100 && echo "B OK"
nc -z -w2 192.168.1.3 9101 && echo "C OK"

# Statut via le controller
python3 -m controller.main status \
    --stores "192.168.1.2:9100,192.168.1.3:9101"
```

### Activation userfaultfd sans root (si nécessaire)

```bash
# Sur nœud A
echo 1 | sudo tee /proc/sys/vm/unprivileged_userfaultfd

# Persistant (à ajouter dans /etc/sysctl.d/99-omega.conf)
echo "vm.unprivileged_userfaultfd = 1" | sudo tee /etc/sysctl.d/99-omega.conf
sudo sysctl -p /etc/sysctl.d/99-omega.conf
```

### Lancement de l'agent sur nœud A

```bash
# Mode demo : scénario de validation
./target/release/node-a-agent \
    --stores "192.168.1.2:9100,192.168.1.3:9101" \
    --vm-id 100 \
    --region-mib 64 \
    --mode demo

# Mode daemon : reste actif
./target/release/node-a-agent \
    --stores "192.168.1.2:9100,192.168.1.3:9101" \
    --vm-id 100 \
    --region-mib 512 \
    --mode daemon
```

---

## Expérience 3 : Test sous pression mémoire

### Étapes

```bash
# Terminal 1 : démarrer le monitoring
./scripts/show_meminfo.sh --watch 3

# Terminal 2 : monitoring PSI
./scripts/show_pressure.sh --watch 2

# Terminal 3 : controller en monitoring
python3 -m controller.main monitor \
    --interval 5 \
    --stores "192.168.1.2:9100,192.168.1.3:9101" \
    --threshold-enable 60 \
    --threshold-migrate 85

# Terminal 4 : baseline avant stress
./scripts/collect_baseline.sh --tag pre-stress

# Lancement du stress
./scripts/run_stress.sh --vm 4 --vm-bytes 70% --timeout 120

# Terminal 4 (suite) : baseline après stress
./scripts/collect_baseline.sh --tag post-stress
```

### Métriques attendues

| Métrique                | Valeur attendue                        |
|-------------------------|----------------------------------------|
| Page faults agent       | >0 (proportionnel aux pages évinvées)  |
| Hit rate store          | ~100% (les pages sont dans le store)   |
| Latence par page fault  | <5ms sur GbE local                     |
| Débit de PUT_PAGE       | >100 MB/s sur GbE                      |

---

## Expérience 4 : Mesure de la latence par page fault

```bash
# Activer les logs debug pour mesurer les timestamps
RUST_LOG=debug ./target/release/node-a-agent \
    --stores "127.0.0.1:9100,127.0.0.1:9101" \
    --mode demo 2>&1 | grep -E "(page fault|GET_PAGE|UFFDIO_COPY)"
```

Calculer le delta timestamp entre "page fault interceptée" et "page injectée avec succès".

---

## Expérience 5 : Répartition des pages entre B et C

```bash
# Après une série de PUT_PAGE, interroger les stats de B et C
python3 -m controller.main status \
    --stores "192.168.1.2:9100,192.168.1.3:9101"
```

Distribution attendue :
- Pages paires → store B (page_id % 2 == 0)
- Pages impaires → store C (page_id % 2 == 1)
- Répartition globale ≈ 50/50 sur 3 nœuds avec page_ids séquentiels

---

## Indicateurs de succès V1

| Critère                                    | Attendu          |
|--------------------------------------------|------------------|
| Scénario demo passe sans erreur d'intégrité | ✓               |
| Latence GET_PAGE sur localhost              | < 1ms            |
| Latence GET_PAGE sur GbE                   | < 3ms            |
| Débit PUT_PAGE                             | > 100 MB/s local |
| Stabilité sur 1000 pages évinvées/récupérées | 0 erreur       |
| Controller décide correctement à 70% RAM   | enable_remote    |
| Tests unitaires Python                     | 100% pass        |
| Tests protocole Rust                       | 100% pass        |
