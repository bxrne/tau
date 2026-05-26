//! Disk-persisted backend for the store.
//!
//! Binary format:
//!   Header:  MAGIC (3 bytes: "TAU") + VERSION (1 byte) + checksum (4 bytes)
//!   Entries: layer_id (8 bytes, u64) + lens_name_len (4 bytes) + lens_name +
//!            tau_count (4 bytes) + repeated taus
//!            Each tau: start (8 bytes, i64) + end (8 bytes, i64) + value (8 bytes, i64)
//!
//! Checksums cover all bytes after the checksum field itself.

use crc32fast::Hasher;

use crate::libtau::crypto;
use crate::libtau::model::{Layer, LayerId, Tau};
use crate::libtau::storage::store::{COMPACT_THRESHOLD, Store, compact_layers};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8] = b"TAU";
const MAGIC_ENC: &[u8] = b"TAUE";
const VERSION: u8 = 1;
const HEADER_OVERHEAD: usize = MAGIC.len() + 1; // + version

/// Compute CRC32 over bytes.
fn checksum(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Serialize a string to bytes with length prefix.
fn write_str<W: Write>(writer: &mut W, s: &str) -> io::Result<()> {
    writer.write_all(&(s.len() as u32).to_le_bytes())?;
    writer.write_all(s.as_bytes())
}

/// Deserialize a string from bytes with length prefix.
fn read_str<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    let result: Result<String, _> = String::from_utf8(buf);
    result.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Trait for binary encoding/decoding of values.
pub trait Codec: Sized {
    /// Encode and write value to writer.
    fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()>;
    /// Read and decode value from reader.
    fn read_encoded<R: Read>(reader: &mut R) -> io::Result<Self>;
}

macro_rules! impl_codec_binary {
    ($($t:ty),+) => {
        $(impl Codec for $t {
            fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()> {
                writer.write_all(&(*self as i64).to_le_bytes())
            }
            fn read_encoded<R: Read>(reader: &mut R) -> io::Result<Self> {
                let mut buf = [0u8; 8];
                reader.read_exact(&mut buf)?;
                Ok(i64::from_le_bytes(buf) as $t)
            }
        })+
    };
}

impl_codec_binary!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, f32, f64);

impl Codec for bool {
    fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&(*self as u8).to_le_bytes())
    }
    fn read_encoded<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;
        Ok(buf[0] != 0)
    }
}

/// One entry in the disk store.
#[derive(Debug)]
struct DiskEntry<V> {
    layer_id: LayerId,
    lens: String,
    taus: Vec<Tau<V>>,
}

impl<V: Codec> DiskEntry<V> {
    fn write<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.layer_id.to_le_bytes())?;
        write_str(writer, &self.lens)?;
        writer.write_all(&(self.taus.len() as u32).to_le_bytes())?;
        for tau in &self.taus {
            writer.write_all(&tau.start.to_le_bytes())?;
            writer.write_all(&tau.end.to_le_bytes())?;
            tau.value.write_encoded(writer)?;
        }
        Ok(())
    }

    fn read(reader: &mut impl Read) -> io::Result<Self> {
        let mut layer_id_buf = [0u8; 8];
        reader.read_exact(&mut layer_id_buf)?;
        let layer_id = u64::from_le_bytes(layer_id_buf);

        let lens = read_str(reader)?;

        let mut count_buf = [0u8; 4];
        reader.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;

        let mut taus = Vec::with_capacity(count);
        for _ in 0..count {
            let mut start_buf = [0u8; 8];
            reader.read_exact(&mut start_buf)?;
            let start = i64::from_le_bytes(start_buf);

            let mut end_buf = [0u8; 8];
            reader.read_exact(&mut end_buf)?;
            let end = i64::from_le_bytes(end_buf);

            let value = V::read_encoded(reader)?;
            taus.push(Tau::new(start, end, value));
        }

        Ok(DiskEntry {
            layer_id,
            lens,
            taus,
        })
    }
}

/// Disk-backed implementation of the Store trait.
pub struct Disk<V> {
    path: PathBuf,
    lenses: HashMap<String, Vec<Layer<V>>>,
    compact_threshold: usize,
    key: Option<[u8; 32]>,
    /// Open file handle for O(entry) appends.  `None` when encryption is
    /// enabled - encrypted files are written atomically via `flush()`.
    append_writer: Option<BufWriter<File>>,
    /// When `false`, [`Disk::append`] skips the per-record `flush + sync_data`
    /// and the caller must call [`Disk::sync`] (or rely on drop) to commit.
    /// Defaults to `true`.
    fsync_each: bool,
    /// When `false`, post-compaction rewrites are skipped - the on-disk file
    /// keeps growing past the live set, but each `append` avoids the
    /// `close fd + flush + reopen` round-trip.  Defaults to `true`.
    rewrite_on_compact: bool,
    /// Reusable byte buffer used to batch one record into a single `write_all`
    /// call (instead of 7+ small calls into the BufWriter).  Cleared between
    /// records; capacity grows to the largest record seen.
    record_buf: Vec<u8>,
}

impl<V: Clone + Codec> Disk<V> {
    /// Open existing store or create new one at `path`.
    ///
    /// Pass `Some(key)` to enable AES-256-GCM encryption at rest. If the file
    /// starts with the `TAUE` magic it is decrypted before parsing; an
    /// unencrypted `TAU` file is always readable regardless of `key`.
    pub fn open(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut lenses: HashMap<String, Vec<Layer<V>>> = HashMap::new();

        if path.exists() {
            let file = File::open(&path)?;
            let mut file_reader = BufReader::new(file);

            // Peek at the first 4 bytes to detect encryption.
            let mut magic4 = [0u8; 4];
            file_reader.read_exact(&mut magic4)?;

            let mut reader: Box<dyn Read> = if magic4 == MAGIC_ENC {
                // Encrypted format: decrypt the remainder, wrap in a Cursor.
                let enc_key = key.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "disk file is encrypted but no TAU_ENCRYPTION_KEY is set",
                    )
                })?;
                let mut blob = Vec::new();
                file_reader.read_to_end(&mut blob)?;
                let plaintext = crypto::decrypt(&enc_key, &blob)?;
                Box::new(Cursor::new(plaintext))
            } else {
                // Unencrypted format: the 4 bytes we already read are TAU + version.
                // Push them back via a chain reader.
                Box::new(Read::chain(Cursor::new(magic4.to_vec()), file_reader))
            };

            // Read and verify the standard 8-byte header (TAU + version + CRC32).
            let mut header = [0u8; HEADER_OVERHEAD + 4];
            reader.read_exact(&mut header)?;

            let (magic, version, stored_checksum) = (
                &header[0..3],
                header[3],
                u32::from_le_bytes(header[4..8].try_into().unwrap()),
            );

            if magic != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid magic: expected TAU, got {:?}",
                        String::from_utf8_lossy(magic)
                    ),
                ));
            }
            if version != VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported version: expected {}, got {}", VERSION, version),
                ));
            }

            let computed_checksum = checksum(&header[0..HEADER_OVERHEAD]);
            if stored_checksum != computed_checksum {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "header checksum mismatch: expected {}, got {}",
                        stored_checksum, computed_checksum
                    ),
                ));
            }

            // Replay entries
            loop {
                match DiskEntry::read(&mut reader) {
                    Ok(entry) => {
                        lenses
                            .entry(entry.lens.clone())
                            .or_default()
                            .push(Layer::new(entry.layer_id, entry.taus));
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e),
                }
            }
        }

        let append_writer = if key.is_none() {
            let f = OpenOptions::new().append(true).open(&path)?;
            Some(BufWriter::with_capacity(256 * 1024, f))
        } else {
            None
        };

        Ok(Self {
            path,
            lenses,
            compact_threshold: COMPACT_THRESHOLD,
            key,
            append_writer,
            fsync_each: true,
            rewrite_on_compact: true,
            record_buf: Vec::with_capacity(256),
        })
    }

    /// Create a new store at `path`. Pass `Some(key)` to encrypt at rest.
    pub fn create(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut plain_header = Vec::new();
        plain_header.extend_from_slice(MAGIC);
        plain_header.push(VERSION);
        let hck = checksum(&plain_header);
        plain_header.extend_from_slice(&hck.to_le_bytes());

        let mut file = File::create(&path)?;
        if let Some(ref enc_key) = key {
            let blob = crypto::encrypt(enc_key, &plain_header);
            file.write_all(MAGIC_ENC)?;
            file.write_all(&blob)?;
        } else {
            file.write_all(&plain_header)?;
        }
        file.sync_data()?;

        let append_writer = if key.is_none() {
            let f = OpenOptions::new().append(true).open(&path)?;
            Some(BufWriter::with_capacity(256 * 1024, f))
        } else {
            None
        };

        Ok(Self {
            path,
            lenses: HashMap::new(),
            compact_threshold: COMPACT_THRESHOLD,
            key,
            append_writer,
            fsync_each: true,
            rewrite_on_compact: true,
            record_buf: Vec::with_capacity(256),
        })
    }

    /// Toggle per-record `fsync` for the append path.  When `false`,
    /// [`Disk::append`] still buffers the write but skips `flush + sync_data`;
    /// the caller must call [`Disk::sync`] (or drop the store) for durability.
    pub fn set_fsync_each(&mut self, on: bool) {
        self.fsync_each = on;
    }

    /// Toggle whether the file is rewritten after each compaction.  When
    /// disabled, the file grows past the live layer set but each `append`
    /// avoids the close/flush/reopen round-trip.  Call [`Disk::flush`]
    /// manually to compact the file.
    pub fn set_rewrite_on_compact(&mut self, on: bool) {
        self.rewrite_on_compact = on;
    }

    /// Flush the buffered append writer and `sync_data` the file.  No-op when
    /// no append writer exists (encrypted backends always rewrite atomically).
    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(w) = &mut self.append_writer {
            w.flush()?;
            w.get_ref().sync_data()?;
        }
        Ok(())
    }

    /// Flush all in-memory state to disk.
    pub fn flush(&self) -> io::Result<()> {
        // Build the standard unencrypted payload in memory.
        let mut payload = Vec::new();

        let mut header = Vec::new();
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        let hck = checksum(&header);
        header.extend_from_slice(&hck.to_le_bytes());
        payload.extend_from_slice(&header);

        for (lens_name, layers) in &self.lenses {
            for layer in layers {
                let entry = DiskEntry {
                    layer_id: layer.id,
                    lens: lens_name.clone(),
                    taus: layer
                        .taus
                        .iter()
                        .map(|t| Tau::new(t.start, t.end, t.value.clone()))
                        .collect(),
                };
                DiskEntry::write(&entry, &mut payload)?;
            }
        }

        let mut file = File::create(&self.path)?;
        if let Some(ref enc_key) = self.key {
            let blob = crypto::encrypt(enc_key, &payload);
            file.write_all(MAGIC_ENC)?;
            file.write_all(&blob)?;
        } else {
            file.write_all(&payload)?;
        }
        file.sync_data()?;
        Ok(())
    }
}

impl<V: Clone + PartialEq + Codec + Send + Sync + 'static> Store<V> for Disk<V> {
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool> {
        let fsync_each = self.fsync_each;
        // Write to disk in O(entry) time when unencrypted append mode is active.
        // Split-borrow self so we can mutate `record_buf` and `append_writer`
        // concurrently.
        let Self {
            append_writer,
            record_buf,
            ..
        } = self;
        if let Some(writer) = append_writer.as_mut() {
            // Batch the whole record into a reusable buffer, then issue a
            // single write_all into the BufWriter.  Avoids the 4+ syscall-shaped
            // small write_all calls that each pay BufWriter indirection cost.
            record_buf.clear();
            record_buf.extend_from_slice(&layer.id.to_le_bytes());
            record_buf.extend_from_slice(&(lens.len() as u32).to_le_bytes());
            record_buf.extend_from_slice(lens.as_bytes());
            record_buf.extend_from_slice(&(layer.taus.len() as u32).to_le_bytes());
            for tau in layer.taus.iter() {
                record_buf.extend_from_slice(&tau.start.to_le_bytes());
                record_buf.extend_from_slice(&tau.end.to_le_bytes());
                tau.value.write_encoded(record_buf)?;
            }
            writer.write_all(record_buf)?;
            if fsync_each {
                writer.flush()?;
                writer.get_ref().sync_data()?;
            }
        }

        let layers = if let Some(l) = self.lenses.get_mut(lens) {
            l
        } else {
            self.lenses.entry(lens.to_string()).or_default()
        };
        let before = layers.len();
        layers.push(layer);
        let mut did_compact = false;
        if layers.len() > self.compact_threshold {
            compact_layers(layers);
            did_compact = layers.len() < before + 1;
        }

        // After compaction, rewrite the file to drop superseded entries.
        if did_compact && self.rewrite_on_compact && self.append_writer.is_some() {
            self.append_writer = None; // close old fd before rewrite
            self.flush()?;
            let f = OpenOptions::new().append(true).open(&self.path)?;
            self.append_writer = Some(BufWriter::new(f));
        }

        Ok(did_compact)
    }

    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>> {
        self.lenses.get(lens)
    }

    fn lens_names(&self) -> Vec<String> {
        self.lenses.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::model::Tau;
    use tempfile::NamedTempFile;

    fn layer(id: u64, items: &[(i64, i64, i32)]) -> Layer<i32> {
        Layer::new(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
        )
    }

    #[test]
    fn create_and_append() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store = Disk::create(tmp.path(), None).unwrap();
        store.append("x", layer(1, &[(0, 10, 42)])).unwrap();
        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 10), None);
    }

    #[test]
    fn open_replays_data() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), None).unwrap();
            store.append("x", layer(1, &[(0, 10, 42)])).unwrap();
            store.flush().unwrap();
        }

        let store = Disk::open(tmp.path(), None).unwrap();
        assert_eq!(store.at("x", 5), Some(42));
    }

    #[test]
    fn multiple_layers_shadow() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), None).unwrap();
            store.append("s", layer(1, &[(0, 20, 1)])).unwrap();
            store.append("s", layer(2, &[(5, 15, 2)])).unwrap();
            store.flush().unwrap();
        }

        let store = Disk::open(tmp.path(), None).unwrap();
        assert_eq!(store.at("s", 3), Some(1));
        assert_eq!(store.at("s", 7), Some(2));
        assert_eq!(store.at("s", 17), Some(1));
    }

    #[test]
    fn multiple_lenses_independent() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), None).unwrap();
            store.append("a", layer(1, &[(0, 10, 1)])).unwrap();
            store.append("b", layer(1, &[(0, 10, 2)])).unwrap();
            store.flush().unwrap();
        }

        let store = Disk::open(tmp.path(), None).unwrap();
        assert_eq!(store.at("a", 5), Some(1));
        assert_eq!(store.at("b", 5), Some(2));
    }

    #[test]
    fn invalid_magic_returns_error() {
        let tmp = NamedTempFile::new().unwrap();
        // Write garbage header
        fs::write(tmp.path(), b"GARB").unwrap();
        let result = Disk::<i32>::open(tmp.path(), None);
        assert!(result.is_err());
    }

    #[test]
    fn encrypted_create_and_flush_roundtrip() {
        let key = [0x77u8; 32];
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), Some(key)).unwrap();
            store.append("x", layer(1, &[(0, 10, 42)])).unwrap();
            store.flush().unwrap();
        }

        // Raw file must start with TAUE magic
        let raw = fs::read(tmp.path()).unwrap();
        assert_eq!(&raw[..4], b"TAUE", "encrypted file must start with TAUE");

        // Re-open with key - data must be intact
        let store = Disk::open(tmp.path(), Some(key)).unwrap();
        assert_eq!(store.at("x", 5), Some(42));
    }

    #[test]
    fn encrypted_open_without_key_returns_error() {
        let key = [0x88u8; 32];
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), Some(key)).unwrap();
            store.append("x", layer(1, &[(0, 10, 1)])).unwrap();
            store.flush().unwrap();
        }

        let result = Disk::<i32>::open(tmp.path(), None);
        assert!(
            result.is_err(),
            "opening encrypted file without key must fail"
        );
    }
}
