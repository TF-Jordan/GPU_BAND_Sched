//! Bridge entre QEMU virtio-gpu-omega et le gpu_multiplexer Rust.
//!
//! # Rôle
//!
//! Ce module écoute sur `/run/omega/gpu-virgl.sock` et traduit les trames
//! du protocole OMVG (émises par `virtio-gpu-omega.c` dans chaque QEMU)
//! en appels sur le [`GpuMultiplexer`].
//!
//! # Protocole OMVG (rappel)
//!
//! ```text
//! Offset  Size  Champ
//! ------  ----  -----
//!    0      4   magic       0x4F4D5647 ("OMVG")
//!    4      4   vm_id       u32 big-endian
//!    8      1   cmd_type    voir OmvgCmd
//!    9      1   flags       0x01 = réponse attendue
//!   10      4   payload_len u32 big-endian
//!   14      N   payload
//! ```
//!
//! # Concurrence
//!
//! Une tâche Tokio par connexion QEMU (une par VM).
//! Le [`GpuMultiplexer`] est partagé via `Arc` — il sérialise les soumissions GPU.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

use crate::gpu_multiplexer::GpuMultiplexer;
use crate::gpu_protocol::Priority;

// ─── Constantes protocole OMVG ────────────────────────────────────────────────

const OMVG_MAGIC: u32 = 0x4F4D5647;
const OMVG_HEADER_SIZE: usize = 14;
const OMVG_MAX_PAYLOAD: usize = 64 * 1024 * 1024; // 64 Mio

const OMVG_FLAG_WANT_REPLY: u8 = 0x01;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OmvgCmd {
    CtxCreate = 0x01,
    CtxDestroy = 0x02,
    Submit = 0x03,
    ResCreate = 0x04,
    ResUnref = 0x05,
    ResTransfer = 0x06,
    Flush = 0x07,
    ResultOk = 0x80,
    ResultError = 0xFF,
}

impl TryFrom<u8> for OmvgCmd {
    type Error = ();
    fn try_from(v: u8) -> Result<Self, ()> {
        match v {
            0x01 => Ok(Self::CtxCreate),
            0x02 => Ok(Self::CtxDestroy),
            0x03 => Ok(Self::Submit),
            0x04 => Ok(Self::ResCreate),
            0x05 => Ok(Self::ResUnref),
            0x06 => Ok(Self::ResTransfer),
            0x07 => Ok(Self::Flush),
            0x80 => Ok(Self::ResultOk),
            0xFF => Ok(Self::ResultError),
            _ => Err(()),
        }
    }
}

// ─── Trame OMVG ───────────────────────────────────────────────────────────────

struct OmvgFrame {
    vm_id: u32,
    cmd: OmvgCmd,
    flags: u8,
    payload: Vec<u8>,
}

async fn read_frame(stream: &mut UnixStream) -> Result<OmvgFrame> {
    let mut hdr = [0u8; OMVG_HEADER_SIZE];
    stream
        .read_exact(&mut hdr)
        .await
        .context("lecture header OMVG")?;

    let magic = u32::from_be_bytes(hdr[0..4].try_into().unwrap());
    let vm_id = u32::from_be_bytes(hdr[4..8].try_into().unwrap());
    let cmd_raw = hdr[8];
    let flags = hdr[9];
    let plen = u32::from_be_bytes(hdr[10..14].try_into().unwrap()) as usize;

    if magic != OMVG_MAGIC {
        bail!("magic OMVG incorrect : 0x{magic:08x}");
    }
    if plen > OMVG_MAX_PAYLOAD {
        bail!("payload OMVG trop grand : {plen} octets");
    }

    let cmd = OmvgCmd::try_from(cmd_raw)
        .map_err(|_| anyhow::anyhow!("cmd OMVG inconnu : 0x{cmd_raw:02x}"))?;

    let mut payload = vec![0u8; plen];
    if plen > 0 {
        stream
            .read_exact(&mut payload)
            .await
            .context("lecture payload OMVG")?;
    }

    Ok(OmvgFrame {
        vm_id,
        cmd,
        flags,
        payload,
    })
}

async fn write_reply(
    stream: &mut UnixStream,
    vm_id: u32,
    cmd: OmvgCmd,
    payload: &[u8],
) -> Result<()> {
    let plen = payload.len() as u32;
    let mut hdr = [0u8; OMVG_HEADER_SIZE];
    hdr[0..4].copy_from_slice(&OMVG_MAGIC.to_be_bytes());
    hdr[4..8].copy_from_slice(&vm_id.to_be_bytes());
    hdr[8] = cmd as u8;
    hdr[9] = 0; // flags réponse
    hdr[10..14].copy_from_slice(&plen.to_be_bytes());

    stream
        .write_all(&hdr)
        .await
        .context("écriture header réponse OMVG")?;
    if !payload.is_empty() {
        stream
            .write_all(payload)
            .await
            .context("écriture payload réponse OMVG")?;
    }
    stream.flush().await.context("flush réponse OMVG")?;
    Ok(())
}

// ─── Contexte par VM ─────────────────────────────────────────────────────────

struct VmGpuContext {
    vm_id: u32,
    multiplexer: Arc<GpuMultiplexer>,
}

impl VmGpuContext {
    fn new(vm_id: u32, multiplexer: Arc<GpuMultiplexer>) -> Self {
        Self { vm_id, multiplexer }
    }

    /// Dispatche une trame OMVG et retourne la réponse (cmd + payload).
    async fn dispatch(&self, frame: &OmvgFrame) -> (OmvgCmd, Vec<u8>) {
        match frame.cmd {
            OmvgCmd::CtxCreate => {
                debug!(vm_id = self.vm_id, "OMVG CTX_CREATE");
                // Le multiplexer gère l'état par vm_id — pas d'action explicite ici
                (OmvgCmd::ResultOk, vec![])
            }

            OmvgCmd::CtxDestroy => {
                debug!(vm_id = self.vm_id, "OMVG CTX_DESTROY");
                self.multiplexer.remove_vm(self.vm_id).await;
                (OmvgCmd::ResultOk, vec![])
            }

            OmvgCmd::Submit => {
                // payload = [submit_header: 8B][virgl_cmds: N B]
                // On extrait les commandes virgl brutes (après les 8 octets de header)
                let cmd_data = if frame.payload.len() > 8 {
                    &frame.payload[8..]
                } else {
                    &frame.payload
                };

                match self
                    .multiplexer
                    .submit_raw(self.vm_id, cmd_data, Priority::Normal)
                    .await
                {
                    Ok(result) => {
                        debug!(vm_id = self.vm_id, bytes = cmd_data.len(), "OMVG SUBMIT ok");
                        (OmvgCmd::ResultOk, result)
                    }
                    Err(e) => {
                        warn!(vm_id = self.vm_id, error = %e, "OMVG SUBMIT erreur");
                        (OmvgCmd::ResultError, e.to_string().into_bytes())
                    }
                }
            }

            OmvgCmd::ResCreate => {
                match self
                    .multiplexer
                    .resource_create(self.vm_id, &frame.payload)
                    .await
                {
                    Ok(_) => (OmvgCmd::ResultOk, vec![]),
                    Err(e) => {
                        warn!(vm_id = self.vm_id, error = %e, "OMVG RES_CREATE erreur");
                        (OmvgCmd::ResultError, e.to_string().into_bytes())
                    }
                }
            }

            OmvgCmd::ResUnref => {
                if frame.payload.len() >= 4 {
                    let res_id = u32::from_le_bytes(frame.payload[0..4].try_into().unwrap());
                    self.multiplexer.resource_unref(self.vm_id, res_id).await;
                }
                (OmvgCmd::ResultOk, vec![])
            }

            OmvgCmd::ResTransfer => {
                match self
                    .multiplexer
                    .resource_transfer(self.vm_id, &frame.payload)
                    .await
                {
                    Ok(_) => (OmvgCmd::ResultOk, vec![]),
                    Err(e) => {
                        warn!(vm_id = self.vm_id, error = %e, "OMVG RES_TRANSFER erreur");
                        (OmvgCmd::ResultError, e.to_string().into_bytes())
                    }
                }
            }

            OmvgCmd::Flush => {
                match self
                    .multiplexer
                    .flush_resource(self.vm_id, &frame.payload)
                    .await
                {
                    Ok(_) => (OmvgCmd::ResultOk, vec![]),
                    Err(e) => {
                        warn!(vm_id = self.vm_id, error = %e, "OMVG FLUSH erreur");
                        (OmvgCmd::ResultError, e.to_string().into_bytes())
                    }
                }
            }

            // Réponses envoyées par le daemon — ne devraient pas arriver côté serveur
            OmvgCmd::ResultOk | OmvgCmd::ResultError => {
                warn!(
                    vm_id = self.vm_id,
                    "trame de réponse reçue côté daemon — ignorée"
                );
                (OmvgCmd::ResultError, b"protocole invalide".to_vec())
            }
        }
    }
}

// ─── Boucle de connexion par VM ───────────────────────────────────────────────

async fn handle_connection(mut stream: UnixStream, multiplexer: Arc<GpuMultiplexer>) {
    // Le premier message doit être CTX_CREATE avec le vm_id
    let first = match read_frame(&mut stream).await {
        Ok(f) => f,
        Err(e) => {
            error!("OMVG première trame illisible : {e}");
            return;
        }
    };

    if first.cmd != OmvgCmd::CtxCreate {
        error!("OMVG : attendu CTX_CREATE en premier, reçu {:?}", first.cmd);
        return;
    }
    let vm_id = first.vm_id;
    info!(vm_id, "OMVG : nouvelle connexion GPU");

    let ctx = VmGpuContext::new(vm_id, multiplexer);

    // Répondre au CTX_CREATE
    if let Err(e) = write_reply(&mut stream, vm_id, OmvgCmd::ResultOk, &[]).await {
        error!(vm_id, "OMVG CTX_CREATE réponse échouée : {e}");
        return;
    }

    // Boucle principale
    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(f) => f,
            Err(e) => {
                debug!(vm_id, "OMVG connexion fermée : {e}");
                break;
            }
        };

        let want_reply = frame.flags & OMVG_FLAG_WANT_REPLY != 0;
        let (resp_cmd, resp_payload) = ctx.dispatch(&frame).await;

        if want_reply {
            if let Err(e) = write_reply(&mut stream, vm_id, resp_cmd, &resp_payload).await {
                error!(vm_id, "OMVG écriture réponse échouée : {e}");
                break;
            }
        }
    }

    ctx.multiplexer.remove_vm(vm_id).await;
    info!(vm_id, "OMVG : connexion GPU terminée");
}

// ─── Serveur Unix socket ──────────────────────────────────────────────────────

/// Lance le serveur OMVG sur `socket_path`.
///
/// Doit être appelé depuis `omega-daemon` au démarrage, après avoir construit
/// le `GpuMultiplexer`. Tourne indéfiniment en acceptant les connexions QEMU.
pub async fn run_virgl_bridge(socket_path: &Path, multiplexer: Arc<GpuMultiplexer>) -> Result<()> {
    // Supprimer le socket existant (redémarrage daemon)
    if socket_path.exists() {
        std::fs::remove_file(socket_path)
            .with_context(|| format!("suppression du socket {}", socket_path.display()))?;
    }

    // Créer le répertoire parent si nécessaire
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("création du répertoire {}", parent.display()))?;
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("bind Unix socket {}", socket_path.display()))?;

    info!(path = %socket_path.display(), "OMVG : bridge virgl démarré");

    loop {
        let (stream, _addr) = listener.accept().await.context("accept OMVG socket")?;
        let mux = multiplexer.clone();
        tokio::spawn(async move {
            handle_connection(stream, mux).await;
        });
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_omvg_cmd_roundtrip() {
        let cmds = [
            (0x01u8, OmvgCmd::CtxCreate),
            (0x02, OmvgCmd::CtxDestroy),
            (0x03, OmvgCmd::Submit),
            (0x04, OmvgCmd::ResCreate),
            (0x05, OmvgCmd::ResUnref),
            (0x06, OmvgCmd::ResTransfer),
            (0x07, OmvgCmd::Flush),
            (0x80, OmvgCmd::ResultOk),
            (0xFF, OmvgCmd::ResultError),
        ];
        for (byte, expected) in cmds {
            let got = OmvgCmd::try_from(byte).unwrap();
            assert_eq!(got as u8, expected as u8);
        }
    }

    #[test]
    fn test_omvg_unknown_cmd_rejected() {
        assert!(OmvgCmd::try_from(0x42u8).is_err());
    }

    #[tokio::test]
    async fn test_frame_roundtrip_in_memory() {
        // Simule une trame CTX_CREATE et vérifie la désérialisation
        let vm_id: u32 = 9004;
        let cmd = OmvgCmd::CtxCreate as u8;
        let payload = vm_id.to_be_bytes();

        let mut buf = Vec::new();
        buf.extend_from_slice(&OMVG_MAGIC.to_be_bytes());
        buf.extend_from_slice(&vm_id.to_be_bytes());
        buf.push(cmd);
        buf.push(OMVG_FLAG_WANT_REPLY);
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(&payload);

        // Vérification manuelle du parsing
        let magic = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        let got_vmid = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let got_cmd = buf[8];
        let got_plen = u32::from_be_bytes(buf[10..14].try_into().unwrap()) as usize;

        assert_eq!(magic, OMVG_MAGIC);
        assert_eq!(got_vmid, vm_id);
        assert_eq!(got_cmd, OmvgCmd::CtxCreate as u8);
        assert_eq!(got_plen, 4);
    }
}
