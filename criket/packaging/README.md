# Cricket Debian packaging

This directory builds two `.deb` packages:

- **`cricket-server`** — RPC server, systemd unit, installed on the GPU host (e.g. Proxmox VE 9.1.1).
- **`cricket-client`** — client library and `cricket-run` wrapper, installed in a VM, LXC container, or workstation.

## Prerequisites

- `dpkg-deb` (`apt install dpkg`)
- Pre-built binaries available in one of:
  - `criket/docs/` (current location in this repository)
  - `criket/bin/` (produced by `make` at the repository root)
  - `criket/cpu/` + `criket/submodules/libtirpc/install/lib/`

Required binaries: `cricket-rpc-server`, `cricket-client.so`, `libtirpc.so.3`.

## Build

```bash
cd criket/packaging
./build-packages.sh          # uses Version: field in DEBIAN/control (0.0.1)
# or
./build-packages.sh 0.0.2    # override version
```

The resulting `.deb` files land in `packaging/dist/`:

```
cricket-server_0.0.1_amd64.deb
cricket-client_0.0.1_amd64.deb
```

## Installation

### Server (Proxmox host)

```bash
dpkg -i cricket-server_0.0.1_amd64.deb
systemctl status cricket-rpc
```

`postinst` creates the `cricket` system user, enables `rpcbind`, starts
`cricket-rpc.service`, and symlinks `/usr/local/cuda` to the first detected
CUDA toolkit if missing.

### Client

```bash
dpkg -i cricket-client_0.0.1_amd64.deb
sudoedit /etc/cricket/client.conf   # set REMOTE_GPU_ADDRESS / REMOTE_GPU_PORT
cricket-run nvidia-smi
cricket-run python3 train.py
```

## Layout installed on disk

| Path                                         | Package         |
|----------------------------------------------|-----------------|
| `/usr/local/bin/cricket-rpc-server`          | cricket-server  |
| `/usr/local/lib/cricket/libtirpc.so.3`       | both            |
| `/etc/cricket/server.conf`                   | cricket-server  |
| `/etc/systemd/system/cricket-rpc.service`    | cricket-server  |
| `/etc/profile.d/cricket-server.sh`           | cricket-server  |
| `/usr/local/bin/cricket-run`                 | cricket-client  |
| `/usr/local/lib/cricket/cricket-client.so`   | cricket-client  |
| `/etc/cricket/client.conf`                   | cricket-client  |
| `/etc/profile.d/cricket-client.sh`           | cricket-client  |

## Configuration files

- `/etc/cricket/server.conf` — sourced by the systemd unit (`EnvironmentFile=`).
  - `TRANSPORT`, `RPC_PORT`, `LOG_LEVEL`, `CUDA_HOME`.
- `/etc/cricket/client.conf` — read by `cricket-run` and `cricket-client.sh`.
  - `REMOTE_GPU_ADDRESS`, `REMOTE_GPU_PORT`, `TRANSPORT`, `AUTO_PRELOAD`.

Set `AUTO_PRELOAD=true` in `client.conf` to export `LD_PRELOAD` in every
login shell (opt-in).

## Removal

```bash
# Server
apt remove cricket-server         # keeps /etc/cricket and user
apt purge  cricket-server         # removes everything including the user

# Client
apt remove cricket-client
apt purge  cricket-client
```
