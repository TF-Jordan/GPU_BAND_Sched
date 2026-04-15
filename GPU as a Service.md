# GPU-as-a-Service avec Proxmox et rCUDA

## 🧠 Objectif

Ce projet vise à permettre à plusieurs machines virtuelles (VMs) d’utiliser un GPU physique partagé **sans passthrough ni vGPU**, en utilisant une approche de **GPU Remoting (CUDA over network)**.

L’idée principale est de transformer le GPU en une ressource distante accessible via le réseau, avec un contrôle total côté hôte.

---

## 🏗️ Architecture globale

### 🔹 Composants

* **Node GPU (Host physique)**

  * Machine équipée d’un ou plusieurs GPU NVIDIA
  * Héberge le serveur rCUDA
  * Exécute les requêtes CUDA envoyées par les VMs

* **VMs (Clients)**

  * Machines virtuelles sous Proxmox
  * Sans accès direct au GPU
  * Utilisent une bibliothèque CUDA modifiée (rCUDA client)

* **Réseau interne**

  * Communication entre VMs et serveur GPU
  * Idéalement ≥ 10 Gbps pour de bonnes performances

---

## ⚙️ Fonctionnement

1. Une application dans une VM appelle une fonction CUDA (ex: `cudaMalloc`, `cudaLaunchKernel`)
2. La bibliothèque rCUDA intercepte cet appel
3. La requête est envoyée au serveur GPU via le réseau
4. Le serveur exécute la commande sur le GPU physique
5. Le résultat est renvoyé à la VM

---

## 🔁 Flux de données

VM (Client)
→ Interception API CUDA (rCUDA)
→ Transmission réseau
→ Serveur rCUDA (Host)
→ Exécution GPU
→ Retour des résultats

---

## 🎯 Avantages

* Mutualisation du GPU entre plusieurs VMs
* Aucun besoin de passthrough ou vGPU
* Compatible avec Proxmox
* Contrôle total sur l’utilisation du GPU
* Possibilité d’ajouter une couche de scheduling personnalisée

---

## ⚠️ Limitations

* Latence réseau (impact sur les performances)
* Certaines fonctionnalités CUDA peuvent être partiellement supportées
* Dépendance à la qualité du réseau
* Debugging plus complexe (système distribué)

---

## 🧩 Extension : Scheduler GPU personnalisé

Une couche supplémentaire peut être ajoutée pour gérer l’accès au GPU :

### Fonctionnalités possibles

* File d’attente des requêtes GPU
* Priorisation des VMs
* Limitation de ressources (quotas)
* Allocation dynamique GPU
* Monitoring des jobs

---

## 📊 Monitoring

Intégration possible avec :

* Prometheus
* Grafana

### Métriques utiles

* Utilisation GPU (compute, mémoire)
* Nombre de requêtes par VM
* Temps d’exécution des kernels
* Latence réseau

---

## 🖥️ Déploiement

### 1. Configuration du Node GPU

* Installer les drivers NVIDIA
* Installer et configurer rCUDA server
* Vérifier l’accès GPU local

### 2. Configuration des VMs

* Installer les dépendances CUDA (sans GPU local)
* Installer rCUDA client
* Configurer la connexion au serveur GPU

### 3. Réseau

* Assurer une connectivité rapide et stable
* Optimiser TCP / utiliser RDMA si possible

---

## 🔐 Sécurité

* Isolation réseau entre VMs
* Authentification entre client et serveur rCUDA
* Limitation des accès GPU par VM

---

## 🚀 Cas d’usage

* Environnements multi-utilisateurs
* Clusters IA / Machine Learning
* Traitement batch GPU
* Laboratoires de recherche
* Plateformes de calcul mutualisé

---

## 🧠 Conclusion

Cette architecture transforme un GPU physique en un service réseau partagé, permettant une utilisation flexible, scalable et contrôlée des ressources GPU sans dépendre des solutions propriétaires de virtualisation.

Elle constitue une alternative open et personnalisable aux solutions comme NVIDIA vGPU ou VMware Bitfusion.

---

## 📌 Remarque

Ce système ne virtualise pas le GPU au niveau matériel, mais déporte l’exécution des appels CUDA via le réseau. Il s’agit donc d’un modèle **GPU-as-a-Service**, et non d’un partage matériel natif.



# GPU-as-a-Service avec Proxmox et rCUDA

## 🧠 Objectif

Ce projet permet à plusieurs machines virtuelles (VMs) sous Proxmox d’utiliser un GPU physique partagé **sans passthrough ni vGPU**, en utilisant une approche de **GPU Remoting (CUDA over network)**.

Le système est distribué sous forme de **package `.deb` installable**, facilitant le déploiement et la gestion.

---

## 🏗️ Architecture globale

### 🔹 Composants

* **Node GPU (Host Proxmox)**

  * Machine avec GPU NVIDIA
  * Exécute le serveur GPU + scheduler
  * Fournit le GPU comme service réseau

* **VMs (Clients)**

  * Machines virtuelles sans GPU
  * Utilisent une bibliothèque CUDA proxy

* **Réseau interne**

  * Communication VM ↔ GPU server
  * Recommandé : ≥ 10 Gbps

---

## ⚙️ Fonctionnement

1. Une application dans une VM appelle CUDA
2. Le wrapper CUDA intercepte l’appel
3. La requête est envoyée au serveur GPU
4. Le scheduler décide quand et comment exécuter
5. Le GPU exécute la tâche
6. Le résultat est renvoyé à la VM

---

## 📦 Package `.deb`

Le projet est distribué sous forme de package Debian :

```bash
gpu-remoting.deb
```

---

## 📁 Structure du package

```
gpu-remoting/
├── DEBIAN/
│   └── control
├── usr/
│   ├── bin/
│   │   └── gpu-scheduler
│   ├── lib/
│   │   └── cuda-wrapper/
│   └── share/
├── etc/
│   └── gpu-remoting/
│       └── config.yaml
├── lib/systemd/system/
│   └── gpu-remoting.service
```

---

## 🧩 Composants internes

### 🖥️ 1. GPU Server

* Basé sur rCUDA
* Expose le GPU via réseau
* Gère les requêtes CUDA distantes

---

### 🧠 2. Scheduler GPU

Responsable de :

* File d’attente des jobs GPU
* Priorisation
* Gestion des quotas par VM
* Allocation des ressources

---

### 🔌 3. CUDA Wrapper (Client VM)

* Remplace `libcuda.so`
* Intercepte les appels CUDA
* Redirige vers le serveur GPU

---

### 🧰 4. CLI (gpu-cli)

Outil de gestion :

```bash
gpu-cli status
gpu-cli top
gpu-cli jobs
gpu-cli submit
```

---

## 🚀 Installation

### 🔹 Sur le host Proxmox (Node GPU)

```bash
dpkg -i gpu-remoting.deb
systemctl enable gpu-remoting
systemctl start gpu-remoting
```

Vérification :

```bash
gpu-cli status
```

---

### 🔹 Sur les VMs

* Installer client léger (wrapper CUDA)
* Configurer l’IP du serveur GPU

Exemple config :

```yaml
server: 192.168.1.10
port: 5000
timeout: 30
```

---

## ⚙️ Configuration

Fichier :

```bash
/etc/gpu-remoting/config.yaml
```

Exemple :

```yaml
gpu:
  devices: [0]
scheduler:
  policy: fair
  max_jobs_per_vm: 2
network:
  port: 5000
```

---

## 🔁 Flux de traitement

```
VM → CUDA Wrapper → Réseau → Scheduler → GPU → Résultat → VM
```

---

## 📊 Monitoring

Compatible avec :

* Prometheus
* Grafana

### Métriques exposées

* Utilisation GPU
* Mémoire GPU
* Nombre de jobs actifs
* Latence moyenne
* Répartition par VM

---

## 🔐 Sécurité

* Isolation réseau recommandée
* Authentification client → serveur
* Limitation des accès GPU

---

## ⚠️ Limitations

* Dépendance au réseau
* Latence supplémentaire
* Support CUDA partiel selon implémentation
* Pas adapté au temps réel strict

---

## 🧪 Cas d’usage

* Machine Learning (training batch)
* Environnements multi-utilisateurs
* Clusters GPU économiques
* Laboratoires de recherche

---

## 🧠 Roadmap

### Phase 1

* Package `.deb` fonctionnel
* GPU remoting opérationnel

### Phase 2

* Scheduler avancé
* Monitoring complet

### Phase 3

* Optimisation réseau (RDMA)
* Multi-GPU support

### Phase 4 (optionnel)

* Intégration Proxmox (UI/API)

---

## 🧠 Conclusion

Cette solution transforme un GPU physique en service mutualisé accessible par plusieurs VMs, sans dépendre des technologies propriétaires comme NVIDIA vGPU.

Elle repose sur une architecture modulaire, distribuée et extensible, facilitée par un déploiement simple via package `.deb`.

---

## 📌 Remarque finale

Ce système ne virtualise pas le GPU matériellement. Il implémente un modèle **GPU-as-a-Service**, basé sur la redirection des appels CUDA via le réseau.

