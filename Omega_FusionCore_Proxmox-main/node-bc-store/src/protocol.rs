//! Protocole binaire TCP pour la communication agent ↔ store.
//!
//! # Format de trame (messages scalaires)
//!
//! ```text
//! Offset  Size  Field
//! ------  ----  -----
//!   0       2   magic       0x524D ("RM")
//!   2       1   opcode      voir enum Opcode
//!   3       1   flags       FLAG_COMPRESSED (0x01)
//!   4       4   vm_id       u32 big-endian
//!   8       8   page_id     u64 big-endian
//!  16       4   payload_len u32 big-endian
//!  20       *   payload     payload_len octets
//! ```
//!
//! # Compression LZ4 (FLAG_COMPRESSED = 0x01)
//!
//! Quand `flags & FLAG_COMPRESSED != 0`, le payload est compressé en LZ4 block.
//! Le payload wire est : `[original_len: u32 BE][lz4_data...]`.
//! `Message::try_compress()` retourne une version compressée si elle est plus courte.
//! La décompression est transparente dans `read_from`.
//!
//! Gain typique sur des pages mémoire VM :
//!   - Pages zéro     : 4096 → ~12 octets  (-99%)
//!   - Pages code     : 4096 → ~2048 octets (-50%)
//!   - Pages heap     : 4096 → ~2500 octets (-39%)
//!   - Données random : pas de gain, envoyé sans FLAG_COMPRESSED
//!
//! # Batch PUT (opcode 0x13 / 0x83)
//!
//! Envoyer N pages en une seule trame réduit N RTT à 1 RTT.
//! Format du payload :
//!   `[page_id_0: u64][data_0: PAGE_SIZE] [page_id_1: u64][data_1: PAGE_SIZE]...`
//! Le champ `page_id` de l'en-tête contient N (le nombre de pages).

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAGIC: u16 = 0x524D;
pub const PAGE_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 20;
pub const MAX_PAYLOAD: usize = PAGE_SIZE * 2;

/// Payload maximal d'un BATCH_PUT (64 pages × (8 + 4096) octets).
pub const MAX_BATCH_PAYLOAD: usize = 64 * (8 + PAGE_SIZE);

/// FLAG_COMPRESSED : le payload est compressé LZ4 (block mode, taille prepend).
pub const FLAG_COMPRESSED: u8 = 0x01;

/// Seuil minimal de payload pour tenter la compression (inutile sur < 64 octets).
const COMPRESS_MIN_LEN: usize = 64;

// ─── Opcodes ──────────────────────────────────────────────────────────────────

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    Ping = 0x01,
    PutPage = 0x10,
    GetPage = 0x11,
    DeletePage = 0x12,
    BatchPutPage = 0x13, // ← nouveau : PUT en lot
    StatsRequest = 0x20,

    Pong = 0x02,
    Ok = 0x80,
    NotFound = 0x81,
    Error = 0x82,
    BatchPutOk = 0x83, // ← nouveau : réponse au batch PUT
    StatsResponse = 0x21,
}

impl TryFrom<u8> for Opcode {
    type Error = io::Error;

    fn try_from(v: u8) -> Result<Self, io::Error> {
        match v {
            0x01 => Ok(Opcode::Ping),
            0x02 => Ok(Opcode::Pong),
            0x10 => Ok(Opcode::PutPage),
            0x11 => Ok(Opcode::GetPage),
            0x12 => Ok(Opcode::DeletePage),
            0x13 => Ok(Opcode::BatchPutPage),
            0x20 => Ok(Opcode::StatsRequest),
            0x21 => Ok(Opcode::StatsResponse),
            0x80 => Ok(Opcode::Ok),
            0x81 => Ok(Opcode::NotFound),
            0x82 => Ok(Opcode::Error),
            0x83 => Ok(Opcode::BatchPutOk),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("opcode inconnu : 0x{v:02x}"),
            )),
        }
    }
}

// ─── Message scalaire ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Message {
    pub opcode: Opcode,
    pub flags: u8,
    pub vm_id: u32,
    pub page_id: u64,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn new(opcode: Opcode, vm_id: u32, page_id: u64, payload: Vec<u8>) -> Self {
        Self {
            opcode,
            flags: 0,
            vm_id,
            page_id,
            payload,
        }
    }

    pub fn ping() -> Self {
        Self::new(Opcode::Ping, 0, 0, vec![])
    }
    pub fn pong() -> Self {
        Self::new(Opcode::Pong, 0, 0, vec![])
    }
    pub fn ok(vm_id: u32, page_id: u64) -> Self {
        Self::new(Opcode::Ok, vm_id, page_id, vec![])
    }
    pub fn not_found(vm_id: u32, page_id: u64) -> Self {
        Self::new(Opcode::NotFound, vm_id, page_id, vec![])
    }
    pub fn error_msg(detail: &str) -> Self {
        Self::new(Opcode::Error, 0, 0, detail.as_bytes().to_vec())
    }
    pub fn put_page(vm_id: u32, page_id: u64, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), PAGE_SIZE);
        Self::new(Opcode::PutPage, vm_id, page_id, data)
    }
    pub fn get_page(vm_id: u32, page_id: u64) -> Self {
        Self::new(Opcode::GetPage, vm_id, page_id, vec![])
    }
    pub fn delete_page(vm_id: u32, page_id: u64) -> Self {
        Self::new(Opcode::DeletePage, vm_id, page_id, vec![])
    }
    pub fn stats_request() -> Self {
        Self::new(Opcode::StatsRequest, 0, 0, vec![])
    }
    pub fn stats_response(json: String) -> Self {
        Self::new(Opcode::StatsResponse, 0, 0, json.into_bytes())
    }

    // ── Compression LZ4 ───────────────────────────────────────────────────

    /// Retourne une copie du message avec payload LZ4-compressé si c'est rentable.
    ///
    /// Si la compression n'apporte pas de gain (données déjà compressées, payload
    /// trop court) → retourne `None` et l'appelant envoie le message sans compression.
    pub fn try_compress(&self) -> Option<Self> {
        if self.payload.len() < COMPRESS_MIN_LEN {
            return None;
        }
        let compressed = lz4_flex::compress_prepend_size(&self.payload);
        if compressed.len() >= self.payload.len() {
            return None;
        }
        Some(Self {
            opcode: self.opcode,
            flags: self.flags | FLAG_COMPRESSED,
            vm_id: self.vm_id,
            page_id: self.page_id,
            payload: compressed,
        })
    }

    // ── I/O asynchrone ────────────────────────────────────────────────────

    /// Lit un message depuis un stream. Décompresse automatiquement si FLAG_COMPRESSED.
    pub async fn read_from<R: AsyncReadExt + Unpin>(reader: &mut R) -> io::Result<Self> {
        let mut header = [0u8; HEADER_SIZE];
        reader.read_exact(&mut header).await?;

        let magic = u16::from_be_bytes([header[0], header[1]]);
        if magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("magic incorrect : 0x{magic:04x}"),
            ));
        }

        let opcode = Opcode::try_from(header[2])?;
        let flags = header[3];
        let vm_id = u32::from_be_bytes(header[4..8].try_into().unwrap());
        let page_id = u64::from_be_bytes(header[8..16].try_into().unwrap());
        let payload_len = u32::from_be_bytes(header[16..20].try_into().unwrap()) as usize;

        // Vérification de taille — batch PUT a un plafond plus élevé
        let max = if matches!(opcode, Opcode::BatchPutPage) {
            MAX_BATCH_PAYLOAD
        } else {
            MAX_PAYLOAD
        };
        if payload_len > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload trop grand : {payload_len} octets (max {max})"),
            ));
        }

        let mut raw = vec![0u8; payload_len];
        if payload_len > 0 {
            reader.read_exact(&mut raw).await?;
        }

        // Décompression transparente — le flag est effacé après décompression
        let (payload, flags) = if flags & FLAG_COMPRESSED != 0 && !raw.is_empty() {
            let data = lz4_flex::decompress_size_prepended(&raw).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("LZ4 décompression : {e}"),
                )
            })?;
            (data, flags & !FLAG_COMPRESSED)
        } else {
            (raw, flags)
        };

        Ok(Self {
            opcode,
            flags,
            vm_id,
            page_id,
            payload,
        })
    }

    /// Écrit un message sur un stream. Ne compresse pas — utilisez `try_compress()` avant.
    pub async fn write_to<W: AsyncWriteExt + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let payload_len = self.payload.len() as u32;
        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len());
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.push(self.opcode as u8);
        buf.push(self.flags);
        buf.extend_from_slice(&self.vm_id.to_be_bytes());
        buf.extend_from_slice(&self.page_id.to_be_bytes());
        buf.extend_from_slice(&payload_len.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        writer.write_all(&buf).await?;
        writer.flush().await
    }
}

// ─── Batch PUT ────────────────────────────────────────────────────────────────

/// N pages à stocker en un seul aller-retour réseau.
///
/// Envoie une unique trame BATCH_PUT_PAGE qui évite N RTT pour N pages.
/// Gain typique sur une éviction CLOCK de 16 pages : latence ÷ 16.
pub struct BatchPutRequest {
    pub vm_id: u32,
    pub pages: Vec<(u64, Vec<u8>)>, // (page_id, data)
}

/// Réponse du store à un BATCH_PUT_PAGE.
pub struct BatchPutResponse {
    pub vm_id: u32,
    pub stored: u32, // pages effectivement stockées
    pub failed: u32, // pages refusées (taille incorrecte, store plein...)
}

impl BatchPutRequest {
    pub fn new(vm_id: u32) -> Self {
        Self {
            vm_id,
            pages: Vec::new(),
        }
    }

    pub fn push(&mut self, page_id: u64, data: Vec<u8>) {
        self.pages.push((page_id, data));
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Sérialise et envoie la requête batch.
    pub async fn write_to<W: AsyncWriteExt + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let count = self.pages.len() as u64;
        // Payload : [(page_id: u64)(data: PAGE_SIZE)] × N
        let mut payload = Vec::with_capacity(self.pages.len() * (8 + PAGE_SIZE));
        for (pid, data) in &self.pages {
            payload.extend_from_slice(&pid.to_be_bytes());
            payload.extend_from_slice(data);
        }

        // Tentative de compression du payload batch
        let (flags, wire_payload) = {
            let compressed = lz4_flex::compress_prepend_size(&payload);
            if compressed.len() < payload.len() {
                (FLAG_COMPRESSED, compressed)
            } else {
                (0u8, payload)
            }
        };

        let payload_len = wire_payload.len() as u32;
        let mut buf = Vec::with_capacity(HEADER_SIZE + wire_payload.len());
        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.push(Opcode::BatchPutPage as u8);
        buf.push(flags);
        buf.extend_from_slice(&self.vm_id.to_be_bytes());
        buf.extend_from_slice(&count.to_be_bytes()); // page_id field = count
        buf.extend_from_slice(&payload_len.to_be_bytes());
        buf.extend_from_slice(&wire_payload);
        writer.write_all(&buf).await?;
        writer.flush().await
    }
}

impl BatchPutResponse {
    /// Lit la réponse BATCH_PUT_OK depuis le stream.
    pub async fn read_from<R: AsyncReadExt + Unpin>(reader: &mut R) -> io::Result<Self> {
        let msg = Message::read_from(reader).await?;
        match msg.opcode {
            Opcode::BatchPutOk => {
                // page_id = stored count, flags = 0
                let stored = (msg.page_id & 0xFFFF_FFFF) as u32;
                let failed = ((msg.page_id >> 32) & 0xFFFF_FFFF) as u32;
                Ok(Self {
                    vm_id: msg.vm_id,
                    stored,
                    failed,
                })
            }
            Opcode::Error => {
                let detail = String::from_utf8_lossy(&msg.payload).to_string();
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("BATCH_PUT erreur store : {detail}"),
                ))
            }
            op => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("BATCH_PUT réponse inattendue : {op:?}"),
            )),
        }
    }

    /// Construit la trame BATCH_PUT_OK à envoyer par le serveur.
    pub fn ok_message(vm_id: u32, stored: u32, failed: u32) -> Message {
        // Encode stored + failed dans le champ page_id
        let page_id = ((failed as u64) << 32) | (stored as u64);
        Message::new(Opcode::BatchPutOk, vm_id, page_id, vec![])
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn roundtrip(msg: Message) -> Message {
        let mut buf: Vec<u8> = Vec::new();
        msg.write_to(&mut buf).await.unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        Message::read_from(&mut reader).await.unwrap()
    }

    #[tokio::test]
    async fn test_ping_roundtrip() {
        let got = roundtrip(Message::ping()).await;
        assert_eq!(got.opcode as u8, Opcode::Ping as u8);
        assert!(got.payload.is_empty());
    }

    #[tokio::test]
    async fn test_put_page_roundtrip() {
        let data = vec![0xABu8; PAGE_SIZE];
        let got = roundtrip(Message::put_page(42, 1234, data.clone())).await;
        assert_eq!(got.opcode as u8, Opcode::PutPage as u8);
        assert_eq!(got.vm_id, 42);
        assert_eq!(got.page_id, 1234);
        assert_eq!(got.payload, data);
    }

    #[tokio::test]
    async fn test_bad_magic() {
        let buf = vec![
            0xDEu8,
            0xAD,
            Opcode::Ping as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        let mut reader = BufReader::new(buf.as_slice());
        assert!(Message::read_from(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn test_compression_zero_page() {
        let data = vec![0u8; PAGE_SIZE]; // page zéro — très compressible
        let msg = Message::put_page(1, 0, data.clone());
        let compressed = msg.try_compress().expect("page zéro doit se comprimer");
        assert!(compressed.flags & FLAG_COMPRESSED != 0);
        assert!(compressed.payload.len() < PAGE_SIZE);

        // Roundtrip : compression + décompression transparente
        let mut buf = Vec::new();
        compressed.write_to(&mut buf).await.unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        let decoded = Message::read_from(&mut reader).await.unwrap();
        assert_eq!(
            decoded.payload, data,
            "décompression doit restaurer les données"
        );
        assert_eq!(
            decoded.flags & FLAG_COMPRESSED,
            0,
            "flag effacé après décompression transparente"
        );
    }

    #[tokio::test]
    async fn test_compression_random_no_gain() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // Générer des données pseudo-aléatoires incompressibles
        let mut data = vec![0u8; PAGE_SIZE];
        let mut h = DefaultHasher::new();
        for (i, b) in data.iter_mut().enumerate() {
            i.hash(&mut h);
            *b = (h.finish() & 0xFF) as u8;
        }
        let msg = Message::put_page(1, 0, data);
        // Peut retourner None si incompressible
        // On vérifie juste que ça ne panique pas
        let _ = msg.try_compress();
    }

    #[tokio::test]
    async fn test_batch_put_roundtrip() {
        let mut req = BatchPutRequest::new(42);
        for i in 0..4u64 {
            req.push(i, vec![i as u8; PAGE_SIZE]);
        }
        assert_eq!(req.len(), 4);

        // Sérialisation
        let mut buf = Vec::new();
        req.write_to(&mut buf).await.unwrap();

        // Lire l'en-tête comme un Message normal pour vérifier le format
        let mut reader = BufReader::new(buf.as_slice());
        let msg = Message::read_from(&mut reader).await.unwrap();
        assert_eq!(msg.opcode as u8, Opcode::BatchPutPage as u8);
        assert_eq!(msg.vm_id, 42);
        assert_eq!(msg.page_id, 4); // count
    }

    #[tokio::test]
    async fn test_batch_put_ok_roundtrip() {
        let msg = BatchPutResponse::ok_message(7, 16, 0);
        let mut buf = Vec::new();
        msg.write_to(&mut buf).await.unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        let resp = BatchPutResponse::read_from(&mut reader).await.unwrap();
        assert_eq!(resp.vm_id, 7);
        assert_eq!(resp.stored, 16);
        assert_eq!(resp.failed, 0);
    }
}
