#!/usr/bin/perl
# proxmox_hook.pl — Hookscript Proxmox V3 pour omega-remote-paging
#
# Gère le cycle de vie de omega-qemu-launcher (agent memfd) et notifie
# omega-daemon (monitoring, éviction, nettoyage pages distantes).
#
# INSTALLATION :
#   1. Lancer scripts/omega-proxmox-install.sh — il installe ce fichier et
#      configure le wrapper QEMU automatiquement.
#
#   2. Ou manuellement :
#      cp scripts/proxmox_hook.pl /var/lib/vz/snippets/omega-hook.pl
#      chmod +x /var/lib/vz/snippets/omega-hook.pl
#      qm set {VMID} --hookscript local:snippets/omega-hook.pl
#
# PHASES Proxmox :
#   pre-start   : prépare l'agent memfd (omega-qemu-launcher prepare)
#   post-start  : notifie omega-daemon (monitoring RAM + GPU + CPU)
#   pre-stop    : signale l'arrêt imminent à omega-daemon
#   post-stop   : arrête l'agent memfd + nettoie les pages distantes

use strict;
use warnings;
use POSIX qw(strftime);

my ($vmid, $phase) = @ARGV;
die "Usage: $0 <vmid> <phase>\n" unless defined $vmid && defined $phase;

# ─── Configuration (variables d'environnement ou valeurs par défaut) ──────────

my $OMEGA_CONTROL_HOST      = $ENV{OMEGA_CONTROL_HOST}      // "127.0.0.1";
my $OMEGA_CONTROL_PORT      = $ENV{OMEGA_CONTROL_PORT}      // "9300";
my $OMEGA_LOG_FILE          = $ENV{OMEGA_LOG_FILE}          // "/var/log/omega-hook.log";
my $OMEGA_LAUNCHER_BIN      = $ENV{OMEGA_LAUNCHER_BIN}      // "/usr/local/bin/omega-qemu-launcher";
my $OMEGA_RUN_DIR           = $ENV{OMEGA_RUN_DIR}           // "/var/lib/omega-qemu";
my $OMEGA_START_TIMEOUT     = $ENV{OMEGA_START_TIMEOUT}     // "30";
my $OMEGA_AGENT_STOP_TIMEOUT = $ENV{OMEGA_AGENT_STOP_TIMEOUT} // "15";
my $OMEGA_SKIP_DAEMON_CHECK = $ENV{OMEGA_SKIP_DAEMON_CHECK} // "0";
my $OMEGA_STORE_PORT        = $ENV{OMEGA_STORE_PORT}        // "9100";

# Charger /etc/omega/cluster.env si OMEGA_NODES ou OMEGA_STORES absents
if (!$ENV{OMEGA_STORES} && !$ENV{OMEGA_NODES} && -f '/etc/omega/cluster.env') {
    open my $fh, '<', '/etc/omega/cluster.env' or die "impossible de lire /etc/omega/cluster.env: $!\n";
    while (<$fh>) {
        chomp; next if /^\s*#/ || !/=/;
        my ($k, $v) = split /=/, $_, 2;
        $ENV{$k} = $v unless exists $ENV{$k};
    }
}

# Calculer OMEGA_STORES depuis OMEGA_NODES en s'excluant soi-même
my $OMEGA_STORES = $ENV{OMEGA_STORES};
if (!$OMEGA_STORES && $ENV{OMEGA_NODES}) {
    my $self_ip = `hostname -I 2>/dev/null`; chomp $self_ip; $self_ip =~ s/\s.*//;
    my @nodes   = grep { $_ ne $self_ip } split /,/, $ENV{OMEGA_NODES};
    $OMEGA_STORES = join ',', map { "$_:$OMEGA_STORE_PORT" } @nodes;
}
die "OMEGA_STORES non défini — créer /etc/omega/cluster.env avec OMEGA_NODES=ip1,ip2,ip3\n"
    unless $OMEGA_STORES;

# ─── Helpers ──────────────────────────────────────────────────────────────────

sub log_msg {
    my ($level, $msg) = @_;
    my $ts   = strftime("%Y-%m-%dT%H:%M:%SZ", gmtime);
    my $line = "[$ts] [$level] vmid=$vmid phase=$phase $msg\n";
    print STDERR $line;
    if (open my $fh, '>>', $OMEGA_LOG_FILE) {
        print $fh $line;
        close $fh;
    }
}

sub curl_json {
    my ($method, $path, $body) = @_;
    my $url = "http://${OMEGA_CONTROL_HOST}:${OMEGA_CONTROL_PORT}${path}";
    my @cmd = (
        'curl', '-s', '-f', '-X', $method, $url,
        '-H', 'Content-Type: application/json',
        '--connect-timeout', '2', '--max-time', '5',
    );
    push @cmd, '-d', $body if defined $body;
    # Chaque argument est entouré de guillemets simples pour éviter que le shell
    # découpe "Content-Type: application/json" en deux tokens lors de l'interpolation.
    my $cmd_str = join(' ', map { "'$_'" } @cmd) . " 2>&1";
    my $out = qx($cmd_str);
    return ($? >> 8, $out);
}

# Exécute omega-qemu-launcher avec les arguments donnés.
# Retourne (exit_code, stdout+stderr).
sub run_launcher {
    my (@args) = @_;
    unless (-x $OMEGA_LAUNCHER_BIN) {
        log_msg("WARN", "omega-qemu-launcher introuvable : $OMEGA_LAUNCHER_BIN");
        return (1, "binary not found");
    }
    my $cmd = join(' ', map { "'$_'" } ($OMEGA_LAUNCHER_BIN, @args));
    my $out = qx($cmd 2>&1);
    return ($? >> 8, $out);
}

# Lit la RAM guest de la VM via qm config (champ memory).
sub get_vm_memory_mib {
    my $out = qx(qm config $vmid 2>/dev/null);
    if ($out =~ /^memory:\s*(\d+)/m) {
        return int($1);
    }
    return undef;
}

# ─── pre-start ────────────────────────────────────────────────────────────────
# Démarre l'agent memfd AVANT que QEMU soit lancé.
# omega-qemu-launcher prepare : fork l'agent, attend les métadonnées memfd,
# écrit state.json pour que le wrapper kvm-omega le lise.

sub phase_pre_start {
    log_msg("INFO", "pré-démarrage — préparation agent memfd");

    # Vérification omega-daemon (avertissement seulement — non bloquant)
    unless ($OMEGA_SKIP_DAEMON_CHECK) {
        my ($rc, $out) = curl_json('GET', '/control/status', undef);
        if ($rc != 0) {
            log_msg("WARN", "omega-daemon non joignable sur ${OMEGA_CONTROL_HOST}:${OMEGA_CONTROL_PORT} (monitoring désactivé)");
        } else {
            log_msg("INFO", "omega-daemon disponible");
        }
    }

    my $size_mib = get_vm_memory_mib();
    unless (defined $size_mib) {
        log_msg("WARN", "impossible de lire la RAM de vmid=$vmid via qm config — agent memfd ignoré");
        exit 0;
    }

    log_msg("INFO", "RAM guest = ${size_mib} MiB — lancement agent memfd");

    my ($rc, $out) = run_launcher(
        'prepare',
        '--vm-id',              $vmid,
        '--size-mib',           $size_mib,
        '--stores',             $OMEGA_STORES,
        '--run-dir',            $OMEGA_RUN_DIR,
        '--start-timeout-secs', $OMEGA_START_TIMEOUT,
    );

    if ($rc == 0) {
        log_msg("INFO", "agent memfd prêt pour vmid=$vmid");
    } else {
        log_msg("ERROR", "omega-qemu-launcher prepare a échoué (rc=$rc) : $out");
        # Non fatal : QEMU peut démarrer sans le backend omega (dégradé)
    }
}

# ─── post-start ───────────────────────────────────────────────────────────────
# QEMU tourne. Notifie omega-daemon pour démarrer le monitoring RAM/CPU/GPU.

sub phase_post_start {
    log_msg("INFO", "post-démarrage — notification omega-daemon");

    my ($rc, $out) = curl_json('POST', "/control/evict/$vmid", '{"count":0}');
    if ($rc == 0) {
        log_msg("INFO", "omega-daemon notifié (monitoring activé)");
    } else {
        log_msg("WARN", "omega-daemon non joignable (post-start) : $out");
    }
}

# ─── pre-stop ─────────────────────────────────────────────────────────────────
# La VM va s'arrêter. Signale omega-daemon pour permettre de rapatrier des pages.

sub phase_pre_stop {
    log_msg("INFO", "pré-arrêt — signalement omega-daemon");

    my ($rc, $out) = curl_json('POST', "/control/evict/$vmid",
                               "{\"count\":0,\"vm_id\":$vmid}");
    if ($rc != 0) {
        log_msg("WARN", "impossible de notifier omega-daemon (pre-stop) : $out");
    }
}

# ─── post-stop ────────────────────────────────────────────────────────────────
# QEMU est arrêté. Arrêter l'agent memfd et supprimer les pages distantes.

sub phase_post_stop {
    log_msg("INFO", "post-arrêt — arrêt agent memfd + nettoyage pages distantes");

    # Si la VM a été immédiatement redémarrée (post-stop asynchrone de Proxmox),
    # ne pas tuer le nouvel agent ni supprimer ses fichiers d'état.
    #
    # On vérifie d'abord le pidfile QEMU (/var/run/qemu-server/{vmid}.pid) :
    # il est créé dès que le daemon QEMU démarre, bien avant que qm status
    # soit "running" — c'est la garde la plus précoce contre la course.
    # On vérifie ensuite qm status pour couvrir les états "starting"/"paused".
    my $pidfile = "/var/run/qemu-server/${vmid}.pid";
    if (-f $pidfile) {
        log_msg("WARN", "VM $vmid pidfile présent ($pidfile) — post-stop ignoré (nouvelle instance QEMU en cours)");
        return;
    }
    my $vm_status = qx(qm status $vmid 2>/dev/null);
    if ($vm_status =~ /running|starting|paused/) {
        log_msg("WARN", "VM $vmid statut '$vm_status' — post-stop ignoré (évite de tuer le nouvel agent)");
        return;
    }

    # Petite pause pour laisser le temps à un éventuel redémarrage immédiat
    # d'écrire son pidfile avant que l'on tue l'agent.
    sleep(2);
    if (-f $pidfile) {
        log_msg("WARN", "VM $vmid pidfile apparu pendant la pause — post-stop abandonné");
        return;
    }
    my $vm_status2 = qx(qm status $vmid 2>/dev/null);
    if ($vm_status2 =~ /running|starting|paused/) {
        log_msg("WARN", "VM $vmid statut '$vm_status2' après pause — post-stop abandonné");
        return;
    }

    # 1. Arrêter l'agent et nettoyer l'état local via le launcher.
    my ($rc_cleanup, $out_cleanup) = run_launcher(
        'cleanup',
        '--vm-id',        $vmid,
        '--run-dir',      $OMEGA_RUN_DIR,
        '--timeout-secs', $OMEGA_AGENT_STOP_TIMEOUT,
    );

    if ($rc_cleanup == 0) {
        log_msg("INFO", "agent memfd et état local nettoyés pour vmid=$vmid");
    } else {
        log_msg("WARN", "cleanup agent/état échoué (rc=$rc_cleanup) : $out_cleanup");
    }

    # 2. Nettoyer les pages distantes sur les stores B/C
    my ($rc_del, $out_del) = curl_json('DELETE', "/control/pages/$vmid", undef);
    if ($rc_del == 0) {
        log_msg("INFO", "pages distantes supprimées pour vmid=$vmid");
    } else {
        log_msg("WARN", "nettoyage pages échoué (daemon peut-être arrêté) : $out_del");
    }

    # Le répertoire d'état local est nettoyé par `omega-qemu-launcher cleanup`.
}

# ─── Dispatch ─────────────────────────────────────────────────────────────────

if    ($phase eq 'pre-start')  { phase_pre_start();  }
elsif ($phase eq 'post-start') { phase_post_start(); }
elsif ($phase eq 'pre-stop')   { phase_pre_stop();   }
elsif ($phase eq 'post-stop')  { phase_post_stop();  }
else  { log_msg("DEBUG", "phase '$phase' ignorée"); }

exit 0;
