//! Protocole binaire bas niveau entre les VMs et le GPU multiplexeur.
//!
//! # Format d'un message
//!
//! ```text
//! ┌──────────────┬──────────────┬──────────────┬──────────────┬─────────────┐
//! │  magic (4B)  │ version (1B) │   type (1B)  │  vm_id (4B)  │  seq (4B)  │
//! ├──────────────┴──────────────┴──────────────┴──────────────┴─────────────┤
//! │  payload_len (4B)  │  priority (1B)  │  reserved (3B)                   │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │  payload (payload_len octets)                                            │
//! └──────────────────────────────────────────────────────────────────────────┘
//!
//! Total header : 22 octets (aligné sur 4 octets)
//! ```
//!
//! # Types de messages
//!
//! | Type | Sens       | Description                              |
//! |------|-----------|------------------------------------------|
//! | 0x01 | VM→GPU    | GPU_CMD : commande GPU brute              |
//! | 0x02 | GPU→VM    | GPU_RESULT : résultat d'une commande     |
//! | 0x03 | VM→GPU    | GPU_ALLOC : allouer N octets VRAM        |
//! | 0x04 | GPU→VM    | GPU_ALLOC_RESP : handle VRAM alloué      |
//! | 0x05 | VM→GPU    | GPU_FREE : libérer un handle VRAM        |
//! | 0x06 | Bidirect  | GPU_SYNC : barrière de synchronisation   |
//! | 0xFF | GPU→VM    | GPU_ERROR : erreur d'exécution            |

use std::io::{self, Read, Write};

/// Nombre magique identifiant le protocole Omega GPU.
pub const MAGIC: u32 = 0x4F4D4750; // "OMGP"
pub const VERSION: u8 = 1;

/// Taille fixe du header (octets).
pub const HEADER_SIZE: usize = 22;

/// Type de message GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    GpuCmd = 0x01,
    GpuResult = 0x02,
    GpuAlloc = 0x03,
    GpuAllocResp = 0x04,
    GpuFree = 0x05,
    GpuSync = 0x06,
    GpuError = 0xFF,
}

impl TryFrom<u8> for MsgType {
    type Error = ();
    fn try_from(v: u8) -> Result<Self, ()> {
        match v {
            0x01 => Ok(Self::GpuCmd),
            0x02 => Ok(Self::GpuResult),
            0x03 => Ok(Self::GpuAlloc),
            0x04 => Ok(Self::GpuAllocResp),
            0x05 => Ok(Self::GpuFree),
            0x06 => Ok(Self::GpuSync),
            0xFF => Ok(Self::GpuError),
            _ => Err(()),
        }
    }
}

/// Priorité d'une commande GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
    RealTime = 3,
}

impl From<u8> for Priority {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Low,
            2 => Self::High,
            3 => Self::RealTime,
            _ => Self::Normal,
        }
    }
}

/// Header d'un message GPU (22 octets, format little-endian).
#[derive(Debug, Clone)]
pub struct GpuHeader {
    pub magic: u32,        // 4B — doit être MAGIC
    pub version: u8,       // 1B
    pub msg_type: MsgType, // 1B
    pub vm_id: u32,        // 4B — VM émettrice
    pub seq: u32,          // 4B — numéro de séquence (pour corrélation réponse)
    pub payload_len: u32,  // 4B — taille du payload en octets
    pub priority: Priority, // 1B
                           // 3 octets réservés (rembourrage)
}

impl GpuHeader {
    pub fn new(
        msg_type: MsgType,
        vm_id: u32,
        seq: u32,
        payload_len: u32,
        priority: Priority,
    ) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            msg_type,
            vm_id,
            seq,
            payload_len,
            priority,
        }
    }

    /// Sérialise le header en 22 octets (little-endian).
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4] = self.version;
        buf[5] = self.msg_type as u8;
        buf[6..10].copy_from_slice(&self.vm_id.to_le_bytes());
        buf[10..14].copy_from_slice(&self.seq.to_le_bytes());
        buf[14..18].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[18] = self.priority as u8;
        // buf[19..22] = réservé (zéro)
        buf
    }

    /// Décode un header depuis 22 octets.
    pub fn decode(buf: &[u8; HEADER_SIZE]) -> Result<Self, ProtocolError> {
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(ProtocolError::BadMagic(magic));
        }
        let version = buf[4];
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }

        let msg_type = MsgType::try_from(buf[5]).map_err(|_| ProtocolError::UnknownType(buf[5]))?;
        let vm_id = u32::from_le_bytes(buf[6..10].try_into().unwrap());
        let seq = u32::from_le_bytes(buf[10..14].try_into().unwrap());
        let payload_len = u32::from_le_bytes(buf[14..18].try_into().unwrap());
        let priority = Priority::from(buf[18]);

        Ok(Self {
            magic,
            version,
            msg_type,
            vm_id,
            seq,
            payload_len,
            priority,
        })
    }
}

/// Message GPU complet (header + payload).
#[derive(Debug, Clone)]
pub struct GpuMessage {
    pub header: GpuHeader,
    /// Payload brut (opcodes GPU, paramètres, données de retour…)
    pub payload: Vec<u8>,
}

impl GpuMessage {
    pub fn new(header: GpuHeader, payload: Vec<u8>) -> Self {
        assert_eq!(header.payload_len as usize, payload.len());
        Self { header, payload }
    }

    /// Lit un message complet depuis un stream (header puis payload).
    pub fn read_from<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut hbuf = [0u8; HEADER_SIZE];
        reader.read_exact(&mut hbuf)?;

        let header = GpuHeader::decode(&hbuf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{:?}", e)))?;

        let mut payload = vec![0u8; header.payload_len as usize];
        if !payload.is_empty() {
            reader.read_exact(&mut payload)?;
        }

        Ok(Self { header, payload })
    }

    /// Écrit le message sur un stream.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let hbuf = self.header.encode();
        writer.write_all(&hbuf)?;
        if !self.payload.is_empty() {
            writer.write_all(&self.payload)?;
        }
        writer.flush()
    }

    /// Construit une réponse GPU_RESULT pour cette commande.
    pub fn make_result(&self, result_payload: Vec<u8>) -> Self {
        let header = GpuHeader::new(
            MsgType::GpuResult,
            self.header.vm_id,
            self.header.seq,
            result_payload.len() as u32,
            self.header.priority,
        );
        Self::new(header, result_payload)
    }

    /// Construit un message d'erreur pour cette commande.
    pub fn make_error(&self, code: u32, message: &str) -> Self {
        let mut payload = code.to_le_bytes().to_vec();
        payload.extend_from_slice(message.as_bytes());
        let header = GpuHeader::new(
            MsgType::GpuError,
            self.header.vm_id,
            self.header.seq,
            payload.len() as u32,
            Priority::Normal,
        );
        Self::new(header, payload)
    }
}

// ─── Sous-structures payload ──────────────────────────────────────────────────

/// Payload d'un GPU_ALLOC (VM → daemon) : demande d'allocation VRAM.
#[derive(Debug, Clone)]
pub struct AllocRequest {
    /// Taille demandée (octets)
    pub size_bytes: u64,
    /// Alignement requis (octets, doit être une puissance de 2)
    pub alignment: u32,
}

impl AllocRequest {
    pub const SIZE: usize = 12;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..8].copy_from_slice(&self.size_bytes.to_le_bytes());
        buf[8..12].copy_from_slice(&self.alignment.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let size_bytes = u64::from_le_bytes(buf[0..8].try_into().ok()?);
        let alignment = u32::from_le_bytes(buf[8..12].try_into().ok()?);
        Some(Self {
            size_bytes,
            alignment,
        })
    }
}

/// Payload d'un GPU_ALLOC_RESP (daemon → VM) : handle VRAM alloué.
#[derive(Debug, Clone)]
pub struct AllocResponse {
    /// Handle opaque identifiant la région VRAM (0 = échec)
    pub handle: u64,
    /// Taille réellement allouée (peut être arrondie)
    pub size_bytes: u64,
}

impl AllocResponse {
    pub const SIZE: usize = 16;

    pub fn encode(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..8].copy_from_slice(&self.handle.to_le_bytes());
        buf[8..16].copy_from_slice(&self.size_bytes.to_le_bytes());
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            handle: u64::from_le_bytes(buf[0..8].try_into().ok()?),
            size_bytes: u64::from_le_bytes(buf[8..16].try_into().ok()?),
        })
    }
}

// ─── Erreurs ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum ProtocolError {
    BadMagic(u32),
    UnsupportedVersion(u8),
    UnknownType(u8),
    Io(io::Error),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic(m) => write!(f, "magic invalide: 0x{m:08X}"),
            Self::UnsupportedVersion(v) => write!(f, "version non supportée: {v}"),
            Self::UnknownType(t) => write!(f, "type inconnu: 0x{t:02X}"),
            Self::Io(e) => write!(f, "I/O: {e}"),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_roundtrip() {
        let h = GpuHeader::new(MsgType::GpuCmd, 100, 42, 256, Priority::High);
        let encoded = h.encode();
        let decoded = GpuHeader::decode(&encoded).unwrap();
        assert_eq!(decoded.magic, MAGIC);
        assert_eq!(decoded.version, VERSION);
        assert_eq!(decoded.msg_type, MsgType::GpuCmd);
        assert_eq!(decoded.vm_id, 100);
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.payload_len, 256);
        assert_eq!(decoded.priority, Priority::High);
    }

    #[test]
    fn test_bad_magic_rejected() {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        assert!(matches!(
            GpuHeader::decode(&buf),
            Err(ProtocolError::BadMagic(_))
        ));
    }

    #[test]
    fn test_message_roundtrip() {
        let payload = vec![0x01, 0x02, 0x03, 0x04];
        let header = GpuHeader::new(
            MsgType::GpuCmd,
            1,
            0,
            payload.len() as u32,
            Priority::Normal,
        );
        let msg = GpuMessage::new(header, payload.clone());

        let mut buf = Vec::new();
        msg.write_to(&mut buf).unwrap();

        let decoded = GpuMessage::read_from(&mut buf.as_slice()).unwrap();
        assert_eq!(decoded.header.vm_id, 1);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_make_result() {
        let cmd_payload = vec![0xAB; 8];
        let header = GpuHeader::new(MsgType::GpuCmd, 42, 7, 8, Priority::Normal);
        let cmd = GpuMessage::new(header, cmd_payload);
        let result = cmd.make_result(vec![0xFF; 4]);
        assert_eq!(result.header.msg_type, MsgType::GpuResult);
        assert_eq!(result.header.vm_id, 42);
        assert_eq!(result.header.seq, 7);
    }

    #[test]
    fn test_alloc_request_roundtrip() {
        let req = AllocRequest {
            size_bytes: 1024 * 1024,
            alignment: 4096,
        };
        let enc = req.encode();
        let dec = AllocRequest::decode(&enc).unwrap();
        assert_eq!(dec.size_bytes, 1024 * 1024);
        assert_eq!(dec.alignment, 4096);
    }

    #[test]
    fn test_alloc_response_roundtrip() {
        let resp = AllocResponse {
            handle: 0xDEAD_CAFE_0000_0001,
            size_bytes: 65536,
        };
        let enc = resp.encode();
        let dec = AllocResponse::decode(&enc).unwrap();
        assert_eq!(dec.handle, 0xDEAD_CAFE_0000_0001);
        assert_eq!(dec.size_bytes, 65536);
    }
}
