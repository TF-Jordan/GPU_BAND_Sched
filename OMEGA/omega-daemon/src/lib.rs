pub mod balloon;
pub mod cgroup_cpu_monitor; // Monitor CPU 1 ms (Rust) — remplace cpu_cgroup_monitor.py
pub mod cluster_api;
pub mod config;
pub mod control_api;
pub mod cpu_cgroup; // CPU réel : cgroups v2 (cpu.weight, cpu.max, cpu.stat)
pub mod disk_io_scheduler; // Politique disque locale via io.weight / io.stat
pub mod eviction_engine;
pub mod fault_bus; // L1 — canal IPC uffd ↔ moteur d'éviction
pub mod gpu_drm_backend; // GPU réel : render node DRM (/dev/dri/renderD128)
pub mod gpu_multiplexer; // Multiplexeur GPU bas niveau (une instance par nœud)
pub mod gpu_protocol; // Protocole binaire VM ↔ daemon GPU
pub mod gpu_runtime; // État GPU synchrone exposable via les APIs HTTP
pub mod gpu_virgl_bridge; // Bridge OMVG : QEMU virtio-gpu-omega ↔ gpu_multiplexer
pub mod io_cgroup; // Disque réel : cgroups v2 (io.weight, io.stat, io.pressure)
pub mod node_state;
pub mod policy_engine;
pub mod qmp_vcpu; // vCPU hotplug réel via QMP (device_add / device_del)
pub mod quota; // garantie de non-dépassement mémoire par VM
pub mod store_server;
pub mod tls; // L3 — chiffrement TLS store TCP
pub mod vcpu_scheduler; // Planificateur vCPU élastique (cgroups + QMP)
pub mod vm_migration; // Migration live (chaud) et cold (froid) via qm migrate
pub mod vm_tracker;
