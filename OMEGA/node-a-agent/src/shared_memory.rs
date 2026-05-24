//! Allocation et description d'un backend mémoire partageable.
//!
//! Cette couche ne branche pas encore QEMU directement, mais elle fournit un
//! backend `memfd` propre, partageable et documentable, au lieu de rester sur
//! une simple région anonyme de démonstration.

use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryBackendKind {
    Anonymous,
    Memfd,
}

impl MemoryBackendKind {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "anonymous" => Ok(Self::Anonymous),
            "memfd" => Ok(Self::Memfd),
            other => bail!("backend mémoire inconnu : {other} (valides : anonymous, memfd)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryBackendOptions {
    pub kind: MemoryBackendKind,
    pub memfd_name: String,
}

impl Default for MemoryBackendOptions {
    fn default() -> Self {
        Self {
            kind: MemoryBackendKind::Anonymous,
            memfd_name: "omega-qemu-remote-memory".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryBackendMetadata {
    pub backend: MemoryBackendKind,
    pub size_bytes: usize,
    pub pid: u32,
    pub proc_fd_path: Option<String>,
}

#[derive(Debug)]
pub struct MemoryBackend {
    pub kind: MemoryBackendKind,
    fd: Option<OwnedFd>,
    size_bytes: usize,
}

impl MemoryBackend {
    pub fn allocate(options: &MemoryBackendOptions, size_bytes: usize) -> Result<Self> {
        match options.kind {
            MemoryBackendKind::Anonymous => Ok(Self {
                kind: MemoryBackendKind::Anonymous,
                fd: None,
                size_bytes,
            }),
            MemoryBackendKind::Memfd => {
                let name = CString::new(options.memfd_name.clone())
                    .context("nom memfd invalide (contient un octet nul)")?;

                let fd = unsafe {
                    libc::syscall(
                        libc::SYS_memfd_create,
                        name.as_ptr(),
                        libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                    ) as libc::c_int
                };

                if fd < 0 {
                    let err = std::io::Error::last_os_error();
                    bail!("memfd_create échoué : {err}");
                }

                let truncate_rc = unsafe { libc::ftruncate(fd, size_bytes as libc::off_t) };
                if truncate_rc != 0 {
                    let err = std::io::Error::last_os_error();
                    unsafe {
                        libc::close(fd);
                    }
                    bail!("ftruncate(memfd) échoué : {err}");
                }

                let owned = unsafe { OwnedFd::from_raw_fd(fd) };
                Ok(Self {
                    kind: MemoryBackendKind::Memfd,
                    fd: Some(owned),
                    size_bytes,
                })
            }
        }
    }

    pub fn map(&self) -> Result<*mut u8> {
        let flags = match self.kind {
            MemoryBackendKind::Anonymous => libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            MemoryBackendKind::Memfd => libc::MAP_SHARED,
        };
        let fd = self.fd.as_ref().map(|fd| fd.as_raw_fd()).unwrap_or(-1);

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                self.size_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                0,
            )
        };

        if base == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            bail!("mmap backend mémoire échoué : {err}");
        }

        Ok(base as *mut u8)
    }

    pub fn metadata(&self) -> MemoryBackendMetadata {
        let pid = std::process::id();
        let proc_fd_path = self
            .fd
            .as_ref()
            .map(|fd| format!("/proc/{pid}/fd/{}", fd.as_raw_fd()));

        MemoryBackendMetadata {
            backend: self.kind,
            size_bytes: self.size_bytes,
            pid,
            proc_fd_path,
        }
    }

    pub fn write_metadata_file(&self, path: &std::path::Path) -> Result<()> {
        let meta = self.metadata();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "création du répertoire parent des métadonnées: {}",
                    parent.display()
                )
            })?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&meta)?)
            .with_context(|| format!("écriture des métadonnées backend: {}", path.display()))?;
        Ok(())
    }

    pub fn proc_fd_path(&self) -> Option<PathBuf> {
        self.metadata().proc_fd_path.map(PathBuf::from)
    }
}

use std::os::fd::AsRawFd;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_backend_kind() {
        assert_eq!(
            MemoryBackendKind::parse("anonymous").unwrap(),
            MemoryBackendKind::Anonymous
        );
        assert_eq!(
            MemoryBackendKind::parse("memfd").unwrap(),
            MemoryBackendKind::Memfd
        );
        assert!(MemoryBackendKind::parse("bad").is_err());
    }

    #[test]
    fn test_memfd_backend_exposes_proc_path() {
        let backend = MemoryBackend::allocate(
            &MemoryBackendOptions {
                kind: MemoryBackendKind::Memfd,
                memfd_name: "omega-test".into(),
            },
            4096 * 4,
        )
        .unwrap();

        let meta = backend.metadata();
        assert_eq!(meta.backend, MemoryBackendKind::Memfd);
        let proc_path = meta.proc_fd_path.unwrap();
        assert!(proc_path.contains("/proc/"));
    }
}
