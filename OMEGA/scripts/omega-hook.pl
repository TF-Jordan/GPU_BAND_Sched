#!/usr/bin/perl
# Hookscript Proxmox VE pour l'agent omega-qemu-remote-memory.
#
# Installation (sur chaque nœud du cluster) :
#   cp omega-hook.pl /var/lib/vz/snippets/
#   chmod +x /var/lib/vz/snippets/omega-hook.pl
#   qm set <vmid> --hookscript local:snippets/omega-hook.pl
#
# Pour ajouter un nœud store : ajouter son adresse IP dans @STORE_NODES.
# Les chaînes AGENT_STORES et AGENT_STATUS_ADDRS sont construites automatiquement.

use strict;
use warnings;
use POSIX qw(setsid);

# ═══════════════════════════════════════════════════════════════════════════════
# CONFIGURATION DU CLUSTER
# Définir OMEGA_NODES dans l'environnement (ou dans /etc/omega/cluster.env) :
#   OMEGA_NODES=192.168.1.1,192.168.1.2,192.168.1.3
# Le nœud courant (hostname -I) est automatiquement exclu de la liste des stores.
# ═══════════════════════════════════════════════════════════════════════════════

my $STORE_PORT  = $ENV{OMEGA_STORE_PORT}  // 9100;
my $STATUS_PORT = $ENV{OMEGA_STATUS_PORT} // 9200;

# Charger /etc/omega/cluster.env si OMEGA_NODES n'est pas déjà dans l'env
if (!$ENV{OMEGA_NODES} && -f '/etc/omega/cluster.env') {
    open my $fh, '<', '/etc/omega/cluster.env' or die;
    while (<$fh>) {
        chomp; next if /^\s*#/ || !/=/;
        my ($k, $v) = split /=/, $_, 2;
        $ENV{$k} = $v unless exists $ENV{$k};
    }
}
# Si OMEGA_NODES absent (ex: nœud cible pendant migration live), sortir proprement.
# Le hookscript ne peut rien faire sans configuration — mais ne doit pas bloquer QEMU.
exit 0 unless $ENV{OMEGA_NODES};

# Exclure le nœud courant de la liste des stores
my $self_ip = `hostname -I 2>/dev/null`; chomp $self_ip; $self_ip =~ s/\s.*//;
my @STORE_NODES = grep { $_ ne $self_ip } split /,/, $ENV{OMEGA_NODES};

# ── Chaînes générées automatiquement depuis @STORE_NODES ─────────────────────
my $STORES       = join(',', map { "$_:$STORE_PORT"  } @STORE_NODES);
my $STATUS_ADDRS = join(',', map { "$_:$STATUS_PORT" } @STORE_NODES);

# ── Valeurs par défaut pour toutes les VMs ───────────────────────────────────
# Surchargeables individuellement dans %VM_CONFIG.
my %DEFAULTS = (
    stores                  => $STORES,
    status_addrs            => $STATUS_ADDRS,
    vm_requested_mib        => 2048,
    region_mib              => 2048,
    vm_vcpus                => 1,
    eviction_threshold_mib  => 512,
    eviction_batch_size     => 64,
    eviction_interval_secs  => 5,
    recall_threshold_mib    => 1024,
    recall_batch_size       => 32,
    recall_interval_secs    => 10,
    migration_enabled       => 'true',
    compaction_enabled      => 'false',
    vm_count_hint           => 1,
    recall_priority         => 5,
    cluster_refresh_secs    => 15,
    migration_interval_secs => 30,
    log_level               => 'info',
    store_timeout_ms        => 2000,
);

# ── Configuration par VM (surcharge les DEFAULTS) ────────────────────────────
# Seuls les champs qui diffèrent des DEFAULTS sont nécessaires ici.
# Toutes les VMs non listées sont ignorées (hookscript transparent).
my %VM_CONFIG = (
    100 => {
        vm_requested_mib => 2048,
        vm_vcpus         => 2,
        recall_priority  => 8,     # haute priorité
        vm_count_hint    => 2,     # 2 VMs sur ce nœud
    },
    101 => {
        vm_requested_mib => 4096,
        region_mib       => 4096,
        vm_vcpus         => 4,
        recall_priority  => 5,
        vm_count_hint    => 2,
    },
    # Ajouter d'autres VMs ici.
    # Seuls les champs qui diffèrent des DEFAULTS sont nécessaires.
);

# ═══════════════════════════════════════════════════════════════════════════════
# FIN DE CONFIGURATION — ne pas modifier en dessous sauf besoin avancé
# ═══════════════════════════════════════════════════════════════════════════════

# ─── Variables d'environnement → noms des flags agent ────────────────────────
my %ENV_MAP = (
    stores                  => 'AGENT_STORES',
    status_addrs            => 'AGENT_STATUS_ADDRS',
    vm_requested_mib        => 'AGENT_VM_REQUESTED_MIB',
    region_mib              => 'AGENT_REGION_MIB',
    vm_vcpus                => 'AGENT_VM_VCPUS',
    eviction_threshold_mib  => 'AGENT_EVICTION_THRESHOLD_MIB',
    eviction_batch_size     => 'AGENT_EVICTION_BATCH_SIZE',
    eviction_interval_secs  => 'AGENT_EVICTION_INTERVAL_SECS',
    recall_threshold_mib    => 'AGENT_RECALL_THRESHOLD_MIB',
    recall_batch_size       => 'AGENT_RECALL_BATCH_SIZE',
    recall_interval_secs    => 'AGENT_RECALL_INTERVAL_SECS',
    migration_enabled       => 'AGENT_MIGRATION_ENABLED',
    compaction_enabled      => 'AGENT_COMPACTION_ENABLED',
    vm_count_hint           => 'AGENT_VM_COUNT_HINT',
    recall_priority         => 'AGENT_RECALL_PRIORITY',
    cluster_refresh_secs    => 'AGENT_CLUSTER_REFRESH_SECS',
    migration_interval_secs => 'AGENT_MIGRATION_INTERVAL_SECS',
    log_level               => 'RUST_LOG',
    store_timeout_ms        => 'AGENT_STORE_TIMEOUT_MS',
);

# ─── Constantes ──────────────────────────────────────────────────────────────
my $AGENT_BIN = '/usr/local/bin/node-a-agent';
my $PID_DIR   = '/run/omega-agent';
my $LOG_DIR   = '/var/log/omega-agent';

# ─── Point d'entrée ──────────────────────────────────────────────────────────
die "Usage: $0 <vmid> <phase>\n" unless @ARGV == 2;
my ($vmid, $phase) = @ARGV;

# VM non listée → hookscript transparent (n'interfère pas)
exit 0 unless exists $VM_CONFIG{$vmid};

# Fusionner DEFAULTS + surcharge spécifique à la VM
my %cfg = (%DEFAULTS, %{ $VM_CONFIG{$vmid} });

if    ($phase eq 'post-start') { agent_start($vmid, \%cfg); }
elsif ($phase eq 'pre-stop')   { agent_stop($vmid);          }
elsif ($phase eq 'post-stop')  { agent_cleanup($vmid);       }

exit 0;

# ─── Fonctions ───────────────────────────────────────────────────────────────

sub agent_start {
    my ($vmid, $cfg) = @_;

    my $pidfile = "$PID_DIR/$vmid.pid";
    if (-f $pidfile) {
        open(my $fh, '<', $pidfile) or die;
        my $old_pid = <$fh>; chomp $old_pid; close $fh;
        if ($old_pid && kill(0, $old_pid) == 1) {
            warn "[omega-hook] agent vmid=$vmid déjà en cours (pid=$old_pid)\n";
            return;
        }
    }

    mkdir $PID_DIR unless -d $PID_DIR;
    mkdir $LOG_DIR unless -d $LOG_DIR;

    my %env = (
        AGENT_VM_ID        => $vmid,
        AGENT_CURRENT_NODE => _hostname(),
        AGENT_MODE         => 'daemon',
        AGENT_BACKEND      => 'anonymous',
        AGENT_LOG_FORMAT   => 'text',
    );
    while (my ($key, $env_name) = each %ENV_MAP) {
        $env{$env_name} = $cfg->{$key} if exists $cfg->{$key};
    }

    my $pid = fork();
    die "fork échoué : $!" unless defined $pid;

    if ($pid == 0) {
        setsid();
        open(STDIN,  '<',  '/dev/null')          or die;
        open(STDOUT, '>>', "$LOG_DIR/$vmid.log") or die;
        open(STDERR, '>&STDOUT')                 or die;
        while (my ($k, $v) = each %env) { $ENV{$k} = $v; }
        exec $AGENT_BIN or die "exec échoué : $!";
    }

    open(my $fh, '>', $pidfile) or die "impossible d'écrire $pidfile : $!";
    print $fh "$pid\n"; close $fh;

    my $store_count = scalar @STORE_NODES;
    warn "[omega-hook] agent vmid=$vmid démarré (pid=$pid, ${store_count} stores)\n";
}

sub agent_stop {
    my ($vmid) = @_;
    my $pidfile = "$PID_DIR/$vmid.pid";
    return unless -f $pidfile;

    open(my $fh, '<', $pidfile) or return;
    my $pid = <$fh>; chomp $pid; close $fh;
    return unless $pid;

    kill('TERM', $pid);
    warn "[omega-hook] SIGTERM envoyé à agent vmid=$vmid (pid=$pid)\n";

    for (1..20) {
        last unless kill(0, $pid);
        select(undef, undef, undef, 0.5);
    }
    kill('KILL', $pid) if kill(0, $pid);
}

sub agent_cleanup {
    my ($vmid) = @_;
    unlink "$PID_DIR/$vmid.pid";
}

sub _hostname {
    chomp(my $h = `hostname -s 2>/dev/null || hostname`);
    return $h;
}
