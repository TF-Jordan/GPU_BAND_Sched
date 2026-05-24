#!/usr/bin/env bash
# uninstall.sh — Supprime complètement omega-remote-paging du cluster.
#
# Sur un cluster Proxmox, chaque nœud est identique : il peut héberger des VMs
# ET offrir sa RAM comme store. Le script nettoie donc TOUT sur chaque nœud
# (omega-daemon, node-bc-store, wrapper kvm, hookscript, binaires, certs TLS).
#
# ─── Configuration ────────────────────────────────────────────────────────────
#
#   OMEGA_NODES=pve,pve2,pve3    # tous les nœuds du cluster (obligatoire)
#                                 # si vide : nettoyage local (root requis)
#
# ─── Autres variables ─────────────────────────────────────────────────────────
#   DEPLOY_USER  : utilisateur SSH (défaut : root)
#   DEPLOY_DIR   : répertoire de déploiement (défaut : /opt/omega-remote-paging)
#   INSTALL_DIR  : répertoire des binaires   (défaut : /usr/local/bin)
#   SNIPPETS_DIR : snippets Proxmox          (défaut : /var/lib/vz/snippets)
#   OMEGA_RUN_DIR: état runtime              (défaut : /var/lib/omega-qemu)
#   OMEGA_LOG_DIR: logs                      (défaut : /var/log/omega)
#   SSH_KEY      : clé SSH privée optionnelle pour les nœuds distants
#   DRY_RUN      : =1 pour afficher les actions sans les exécuter

set -euo pipefail

: "${DEPLOY_USER:=root}"
: "${DEPLOY_DIR:=/opt/omega-remote-paging}"
: "${INSTALL_DIR:=/usr/local/bin}"
: "${SNIPPETS_DIR:=/var/lib/vz/snippets}"
: "${HOOKSCRIPT_NAME:=omega-hook.pl}"
: "${OMEGA_RUN_DIR:=/var/lib/omega-qemu}"
: "${OMEGA_LOG_DIR:=/var/log/omega}"
: "${OMEGA_REAL_KVM:=/usr/bin/kvm.real}"
: "${BRIDGE_LIB:=/usr/local/lib/omega-uffd-bridge.so}"
: "${OMEGA_GPU_WORKER_DIR:=/opt/omega-remote-paging/workers}"
: "${SSH_KEY:=}"
: "${DRY_RUN:=0}"

SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
if [[ -n "$SSH_KEY" && -f "$SSH_KEY" ]]; then
    SSH_OPTS=(-i "$SSH_KEY" "${SSH_OPTS[@]}")
fi

info()    { echo -e "\033[32m[INFO]\033[0m  $*"; }
warn()    { echo -e "\033[33m[WARN]\033[0m  $*"; }
success() { echo -e "\033[32m[OK]\033[0m    $*"; }
step()    { echo; echo -e "\033[34m──── $* ────\033[0m"; }

run() {
    if [[ "$DRY_RUN" == "1" ]]; then
        echo -e "\033[90m[DRY]\033[0m   $*"
    else
        eval "$@"
    fi
}

# ─── Corps du nettoyage (exécuté sur chaque nœud) ────────────────────────────

clean_body() { cat <<'BODY'
    # Services
    for svc in omega-daemon node-bc-store omega-store "omega-agent@" omega-agent omega-hookscript-watcher omega-gpu-proxy; do
        systemctl stop    "${svc}.service" 2>/dev/null || true
        systemctl disable "${svc}.service" 2>/dev/null || true
        rm -f "/etc/systemd/system/${svc}.service"
    done
    systemctl daemon-reload

    # Hookscript sur toutes les VMs de ce nœud
    if command -v qm &>/dev/null && command -v pvesh &>/dev/null; then
        HOOKSCRIPT_REF="local:snippets/@@HOOKSCRIPT_NAME@@"
        VMS=$(pvesh get /nodes/$(hostname)/qemu --output-format json 2>/dev/null \
            | python3 -c "import json,sys; vms=json.load(sys.stdin); print(' '.join(str(v['vmid']) for v in vms))" 2>/dev/null || true)
        for vmid in $VMS; do
            HOOK=$(qm config "$vmid" 2>/dev/null | grep -E '^hookscript:' | awk '{print $2}' || true)
            if [[ "$HOOK" == "$HOOKSCRIPT_REF" ]]; then
                echo "[INFO]  Suppression hookscript VM ${vmid}"
                qm set "$vmid" --delete hookscript || true
            fi
        done
    else
        echo "[WARN]  qm/pvesh introuvable — hookscripts non supprimés des VMs"
    fi

    # Restaurer /usr/bin/kvm (chattr -i au cas où le fichier est immutable)
    chattr -i /usr/bin/kvm 2>/dev/null || true
    if [[ -e @@OMEGA_REAL_KVM@@ ]]; then
        rm -f /usr/bin/kvm && mv @@OMEGA_REAL_KVM@@ /usr/bin/kvm \
            && echo "[OK]    /usr/bin/kvm restauré" \
            || echo "[WARN]  impossible de restaurer /usr/bin/kvm — à faire manuellement"
    elif [[ -L /usr/bin/kvm ]]; then
        rm -f /usr/bin/kvm \
            && echo "[OK]    symlink /usr/bin/kvm supprimé" \
            || echo "[WARN]  impossible de supprimer /usr/bin/kvm — à faire manuellement"
    fi

    # Binaires
    rm -f @@INSTALL_DIR@@/omega-qemu-launcher \
          @@INSTALL_DIR@@/node-a-agent \
          @@INSTALL_DIR@@/omega-daemon \
          @@INSTALL_DIR@@/omega-gpu-proxy \
          @@INSTALL_DIR@@/kvm-omega

    # Bridge LD_PRELOAD
    rm -f @@BRIDGE_LIB@@
    ldconfig 2>/dev/null || true

    # Hookscript fichier
    rm -f @@SNIPPETS_DIR@@/@@HOOKSCRIPT_NAME@@

    # Répertoire de déploiement, certs TLS, runtime, logs
    rm -rf @@DEPLOY_DIR@@
    rm -rf @@OMEGA_GPU_WORKER_DIR@@
    rm -rf /etc/omega-store/tls
    rm -rf @@OMEGA_RUN_DIR@@ @@OMEGA_LOG_DIR@@
    rm -f /run/omega-gpu-scheduler-*.lock 2>/dev/null || true
BODY
}

# Injecter les valeurs de configuration dans le corps du script
rendered_body() {
    clean_body \
        | sed "s|@@HOOKSCRIPT_NAME@@|${HOOKSCRIPT_NAME}|g" \
        | sed "s|@@OMEGA_REAL_KVM@@|${OMEGA_REAL_KVM}|g" \
        | sed "s|@@INSTALL_DIR@@|${INSTALL_DIR}|g" \
        | sed "s|@@BRIDGE_LIB@@|${BRIDGE_LIB}|g" \
        | sed "s|@@SNIPPETS_DIR@@|${SNIPPETS_DIR}|g" \
        | sed "s|@@DEPLOY_DIR@@|${DEPLOY_DIR}|g" \
        | sed "s|@@OMEGA_GPU_WORKER_DIR@@|${OMEGA_GPU_WORKER_DIR}|g" \
        | sed "s|@@OMEGA_RUN_DIR@@|${OMEGA_RUN_DIR}|g" \
        | sed "s|@@OMEGA_LOG_DIR@@|${OMEGA_LOG_DIR}|g"
}

clean_node_remote() {
    local node="$1"
    step "Nettoyage ${DEPLOY_USER}@${node}"
    if [[ "$DRY_RUN" == "1" ]]; then
        echo -e "\033[90m[DRY]\033[0m   ssh ${DEPLOY_USER}@${node} ..."
        rendered_body | sed 's/^/    /'
    else
        ssh "${SSH_OPTS[@]}" "${DEPLOY_USER}@${node}" "set -x
$(rendered_body)" && success "Nœud ${node} nettoyé" || warn "Nœud ${node} — nettoyage partiel (voir logs ci-dessus)"
    fi
}

clean_node_local() {
    step "Nettoyage local"
    [[ "$(id -u)" == "0" ]] || { echo "Root requis pour le nettoyage local."; exit 1; }
    eval "$(rendered_body)"
    success "Nœud local nettoyé"
}

# ─── Résoudre la liste des nœuds ──────────────────────────────────────────────

NODES=()
if [[ -n "${OMEGA_NODES:-}" ]]; then
    IFS=',' read -ra NODES <<< "$OMEGA_NODES"
fi

# ─── Main ─────────────────────────────────────────────────────────────────────

echo
echo -e "\033[31m╔══════════════════════════════════════════════════════════════╗\033[0m"
echo -e "\033[31m║       omega-remote-paging — DÉSINSTALLATION DU CLUSTER       ║\033[0m"
echo -e "\033[31m╚══════════════════════════════════════════════════════════════╝\033[0m"
echo
[[ "$DRY_RUN" == "1" ]] && warn "MODE DRY-RUN — aucune action réelle"

if [[ ${#NODES[@]} -eq 0 ]]; then
    info "OMEGA_NODES non défini — nettoyage local"
    clean_node_local
else
    info "Nœuds : ${NODES[*]}"
    echo
    for node in "${NODES[@]}"; do
        clean_node_remote "$node"
    done
fi

echo
echo -e "\033[32m╔══════════════════════════════════════════════════════════════╗\033[0m"
echo -e "\033[32m║            Désinstallation terminée                          ║\033[0m"
echo -e "\033[32m╚══════════════════════════════════════════════════════════════╝\033[0m"
echo
