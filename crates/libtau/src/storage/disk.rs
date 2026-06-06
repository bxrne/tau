//! Disk-persisted backend for the store.
//!
//! Binary format:
//!   Header:  MAGIC (4 bytes: "TAUZ") + VERSION (1 byte) + FLAGS (1 byte) +
//!            CRC32 (4 bytes, covers magic+version+flags)
//!   Body:    zstd-compressed payload; if FLAGS bit 0 is set, the body is
//!            AES-256-GCM encrypted before compression (compress-then-encrypt).
//!   Payload (VERSION >= 2): schema section then entries.
//!     Schema:  schema_count (4 bytes, u32) + repeated DDL strings
//!              Each string: len (4 bytes, u32) + UTF-8 bytes
//!     Entries: layer_id (8 bytes, u64) + lens_name_len (4 bytes) + lens_name +
//!              tau_count (4 bytes) + repeated taus
//!              Each tau: start (8 bytes, i64) + end (8 bytes, i64) + value (encoded)
//!   VERSION 1 files carry no schema section; they still open (schema is empty).

use crate::crypto;
use crate::model::{Layer, LayerId, Tau};
use crate::storage::layers::compact_layers;
use crate::storage::store::{COMPACT_THRESHOLD, Store};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use crc32fast::Hasher;

const MAGIC: &[u8] = b"TAUZ";
/// Current on-disk format version. Version 1 had no schema section; version 2
/// prefixes the entry stream with persisted schema DDL. Both versions open.
const VERSION: u8 = 2;
const FLAG_ENCRYPTED: u8 = 0x01;
const HEADER_LEN: usize = 4 + 1 + 1 + 4; // magic + version + flags + crc32
/// Default zstd compression level. Valid range is 1–22; higher = better ratio, slower.
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

fn checksum(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

fn write_str<W: Write>(writer: &mut W, s: &str) -> io::Result<()> {
    writer.write_all(&(s.len() as u32).to_le_bytes())?;
    writer.write_all(s.as_bytes())
}

fn read_str<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Write the schema section: a `u32` count followed by each length-prefixed string.
fn write_schema<W: Write>(writer: &mut W, schema: &[String]) -> io::Result<()> {
    writer.write_all(&(schema.len() as u32).to_le_bytes())?;
    for stmt in schema {
        write_str(writer, stmt)?;
    }
    Ok(())
}

/// Read the schema section written by [`write_schema`].
fn read_schema<R: Read>(reader: &mut R) -> io::Result<Vec<String>> {
    let mut count_buf = [0u8; 4];
    reader.read_exact(&mut count_buf)?;
    let count = u32::from_le_bytes(count_buf) as usize;
    let mut schema = Vec::with_capacity(count);
    for _ in 0..count {
        schema.push(read_str(reader)?);
    }
    Ok(schema)
}

/// Trait for binary encoding/decoding of values.
pub trait Codec: Sized {
    fn write_encoded<W: Write>(&self, writer: &mut W) -> io::Result<()>;
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

/// Disk-backed implementation of [`Store`].
///
/// All writes are zstd-compressed. Pass `Some(key)` to additionally encrypt
/// with AES-256-GCM (compress then encrypt). The file always begins with the
/// `TAUZ` magic; the FLAGS byte indicates whether encryption is active.
pub struct Disk<V> {
    path: PathBuf,
    lenses: HashMap<String, Vec<Layer<V>>>,
    /// Persisted schema DDL statements in write order. Replayed by the executor
    /// on restart so `CREATE LENS` / `DERIVE LENS` / `SET TTL` survive.
    schema: Vec<String>,
    compact_threshold: usize,
    key: Option<[u8; 32]>,
    compression_level: i32,
}

impl<V: Clone + Codec> Disk<V> {
    /// Open existing store or create new one at `path`.
    pub fn open(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut lenses: HashMap<String, Vec<Layer<V>>> = HashMap::new();
        let mut schema: Vec<String> = Vec::new();

        if path.exists() {
            let file = File::open(&path)?;
            let mut reader = BufReader::new(file);

            let mut header = [0u8; HEADER_LEN];
            reader.read_exact(&mut header)?;

            if &header[0..4] != MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "invalid magic: expected TAUZ, got {:?}",
                        String::from_utf8_lossy(&header[0..4])
                    ),
                ));
            }
            let version = header[4];
            if version == 0 || version > VERSION {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported version: expected 1..={VERSION}, got {version}"),
                ));
            }
            let flags = header[5];
            let stored_crc = u32::from_le_bytes(
                header[6..10]
                    .try_into()
                    .expect("header slice is exactly 4 bytes"),
            );
            let computed_crc = checksum(&header[0..6]);
            if stored_crc != computed_crc {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("header checksum mismatch: expected {stored_crc}, got {computed_crc}"),
                ));
            }

            let mut body = Vec::new();
            reader.read_to_end(&mut body)?;

            let compressed = if flags & FLAG_ENCRYPTED != 0 {
                let enc_key = key.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "disk file is encrypted but no TAU_ENCRYPTION_KEY is set",
                    )
                })?;
                crypto::decrypt(&enc_key, &body)?
            } else {
                body
            };

            let payload_bytes = zstd::decode_all(compressed.as_slice())?;
            let mut cursor = Cursor::new(payload_bytes);
            // Version 2+ prefixes the entry stream with the schema section.
            if version >= 2 {
                schema = read_schema(&mut cursor)?;
            }
            loop {
                match DiskEntry::read(&mut cursor) {
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

        Ok(Self {
            path,
            lenses,
            schema,
            compact_threshold: COMPACT_THRESHOLD,
            key,
            compression_level: DEFAULT_ZSTD_LEVEL,
        })
    }

    /// Create a new store at `path`. Pass `Some(key)` to encrypt at rest.
    pub fn create(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self {
            path,
            lenses: HashMap::new(),
            schema: Vec::new(),
            compact_threshold: COMPACT_THRESHOLD,
            key,
            compression_level: DEFAULT_ZSTD_LEVEL,
        };
        store.flush()?;
        Ok(store)
    }

    /// Override the number of layers per lens that triggers automatic compaction.
    pub fn set_compact_threshold(&mut self, n: usize) {
        self.compact_threshold = n;
    }

    /// Set the zstd compression level used on the next [`Disk::flush`].
    /// Valid range is 1–22 (clamped by zstd internally). Default is [`DEFAULT_ZSTD_LEVEL`].
    pub fn set_compression_level(&mut self, level: i32) {
        self.compression_level = level;
    }

    /// Flush all in-memory state to disk atomically (compress → optionally encrypt → write).
    pub fn flush(&self) -> io::Result<()> {
        let mut entries = Vec::new();
        write_schema(&mut entries, &self.schema)?;
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
                entry.write(&mut entries)?;
            }
        }

        let compressed = zstd::encode_all(entries.as_slice(), self.compression_level)
            .map_err(io::Error::other)?;

        let flags = if self.key.is_some() {
            FLAG_ENCRYPTED
        } else {
            0u8
        };
        let body = if let Some(ref enc_key) = self.key {
            crypto::encrypt(enc_key, &compressed)
        } else {
            compressed
        };

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.push(VERSION);
        header.push(flags);
        let crc = checksum(&header);
        header.extend_from_slice(&crc.to_le_bytes());

        let tmp = self.path.with_extension("tmp");
        {
            let mut file = File::create(&tmp)?;
            file.write_all(&header)?;
            file.write_all(&body)?;
            file.sync_data()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

impl<V: Clone + PartialEq + Codec + Send + Sync + 'static> Store<V> for Disk<V> {
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool> {
        let layers = self.lenses.entry(lens.to_string()).or_default();
        let before = layers.len();
        layers.push(layer);
        let mut did_compact = false;
        if layers.len() > self.compact_threshold {
            compact_layers(layers);
            did_compact = layers.len() < before + 1;
        }
        // Persist on every append: the disk backend is the only durability
        // mechanism when no WAL is attached, so an acknowledged write must hit
        // disk before we return. (`flush` rewrites the whole file atomically.)
        self.flush()?;
        Ok(did_compact)
    }

    fn drop_lens(&mut self, lens: &str) {
        self.lenses.remove(lens);
    }

    fn layers(&self, lens: &str) -> Option<&Vec<Layer<V>>> {
        self.lenses.get(lens)
    }

    fn lens_names(&self) -> Vec<String> {
        self.lenses.keys().cloned().collect()
    }

    fn append_schema(&mut self, stmt: &str) -> io::Result<()> {
        self.schema.push(stmt.to_string());
        // Persist immediately so DDL survives an unclean shutdown.
        self.flush()
    }

    fn schema_stmts(&self) -> Vec<String> {
        self.schema.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tau;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;

    fn taus_gen() -> impl Generator<Vec<Tau<i32>>> {
        gs::vecs(
            gs::integers::<i64>()
                .min_value(1)
                .max_value(50)
                .flat_map(|width| {
                    gs::integers::<i64>()
                        .min_value(0)
                        .max_value(20)
                        .flat_map(move |gap| gs::integers::<i32>().map(move |v| (width, gap, v)))
                }),
        )
        .min_size(1)
        .max_size(8)
        .map(|specs| {
            let mut taus = Vec::with_capacity(specs.len());
            let mut cursor: i64 = 0;
            for (width, gap, v) in specs {
                let s = cursor + gap;
                let e = s + width;
                taus.push(Tau::new(s, e, v));
                cursor = e;
            }
            taus
        })
    }

    fn lens_name_gen() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9_]{0,8}").fullmatch(true)
    }

    #[hegel::test]
    fn pbt_create_append_at_matches_in_memory(tc: TestCase) {
        let lens = tc.draw(lens_name_gen());
        let layer = Layer::new(1, tc.draw(taus_gen()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(2000));
        let tmp = NamedTempFile::new().unwrap();
        let mut store = Disk::create(tmp.path(), None).unwrap();
        store.append(&lens, layer.clone()).unwrap();

        let expected = layer
            .taus
            .iter()
            .find(|t| t.contains(probe))
            .map(|t| t.value);
        assert_eq!(store.at(&lens, probe), expected);
    }

    #[hegel::test]
    fn pbt_open_after_flush_replays_data_unencrypted(tc: TestCase) {
        let lens = tc.draw(lens_name_gen());
        let layer = Layer::new(1, tc.draw(taus_gen()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(2000));
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), None).unwrap();
            store.append(&lens, layer.clone()).unwrap();
            store.flush().unwrap();
        }
        let store: Disk<i32> = Disk::open(tmp.path(), None).unwrap();
        let expected = layer
            .taus
            .iter()
            .find(|t| t.contains(probe))
            .map(|t| t.value);
        assert_eq!(store.at(&lens, probe), expected);
    }

    #[hegel::test]
    fn pbt_open_after_flush_replays_data_encrypted(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let lens = tc.draw(lens_name_gen());
        let layer = Layer::new(1, tc.draw(taus_gen()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(2000));
        let key: [u8; 32] = key_bytes.try_into().expect("exactly 32 bytes");
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), Some(key)).unwrap();
            store.append(&lens, layer.clone()).unwrap();
            store.flush().unwrap();
        }
        // Encrypted file still starts with TAUZ magic; FLAG_ENCRYPTED is in the flags byte.
        let raw = fs::read(tmp.path()).unwrap();
        assert_eq!(&raw[..4], b"TAUZ");
        assert_eq!(raw[5], FLAG_ENCRYPTED);

        let store: Disk<i32> = Disk::open(tmp.path(), Some(key)).unwrap();
        let expected = layer
            .taus
            .iter()
            .find(|t| t.contains(probe))
            .map(|t| t.value);
        assert_eq!(store.at(&lens, probe), expected);
    }

    #[hegel::test]
    fn pbt_encrypted_file_rejects_open_without_key(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let lens = tc.draw(lens_name_gen());
        let layer = Layer::new(1, tc.draw(taus_gen()));
        let key: [u8; 32] = key_bytes.try_into().expect("exactly 32 bytes");
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), Some(key)).unwrap();
            store.append(&lens, layer).unwrap();
            store.flush().unwrap();
        }
        let result: io::Result<Disk<i32>> = Disk::open(tmp.path(), None);
        assert!(result.is_err());
    }

    #[hegel::test]
    fn pbt_encrypted_file_rejects_open_with_wrong_key(tc: TestCase) {
        let lens = tc.draw(lens_name_gen());
        let layer = Layer::new(1, tc.draw(taus_gen()));
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let key_a: [u8; 32] = key_bytes.try_into().expect("exactly 32 bytes");
        let mut key_b = key_a;
        key_b[0] ^= 1;
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), Some(key_a)).unwrap();
            store.append(&lens, layer).unwrap();
            store.flush().unwrap();
        }
        let result: io::Result<Disk<i32>> = Disk::open(tmp.path(), Some(key_b));
        assert!(result.is_err());
    }

    #[hegel::test]
    fn pbt_invalid_magic_rejected(tc: TestCase) {
        let bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(4).max_size(64));
        if &bytes[..4] == b"TAUZ" {
            return; // skip valid prefix
        }
        let tmp = NamedTempFile::new().unwrap();
        fs::write(tmp.path(), &bytes).unwrap();
        let result: io::Result<Disk<i32>> = Disk::open(tmp.path(), None);
        assert!(result.is_err());
    }

    #[hegel::test]
    fn pbt_custom_compression_level_round_trips(tc: TestCase) {
        let level = tc.draw(gs::integers::<i32>().min_value(1).max_value(22));
        let lens = tc.draw(lens_name_gen());
        let layer = Layer::new(1, tc.draw(taus_gen()));
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(2000));
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), None).unwrap();
            store.set_compression_level(level);
            store.append(&lens, layer.clone()).unwrap();
            store.flush().unwrap();
        }
        let store: Disk<i32> = Disk::open(tmp.path(), None).unwrap();
        let expected = layer
            .taus
            .iter()
            .find(|t| t.contains(probe))
            .map(|t| t.value);
        assert_eq!(store.at(&lens, probe), expected);
    }

    #[test]
    fn compression_level_1_produces_smaller_file_than_uncompressed_equivalent() {
        // Sanity check: a large repeated dataset compresses to less than raw entry bytes.
        let tmp = NamedTempFile::new().unwrap();
        let mut store = Disk::<i64>::create(tmp.path(), None).unwrap();
        let taus: Vec<Tau<i64>> = (0..1000i64).map(|i| Tau::new(i, i + 1, 42)).collect();
        store.append("big", Layer::new(1, taus)).unwrap();
        store.flush().unwrap();
        let compressed_size = fs::metadata(tmp.path()).unwrap().len();
        // Raw entries would be 1000 * (8+8+8) = 24_000 bytes plus overhead; compressed must be less.
        assert!(
            compressed_size < 24_000,
            "expected compression, got {compressed_size} bytes"
        );
    }

    #[hegel::test]
    fn pbt_multiple_lenses_round_trip(tc: TestCase) {
        let entries: Vec<(String, Layer<i32>)> = (0..3)
            .map(|i| {
                let mut name = tc.draw(lens_name_gen());
                let layer = Layer::new(1, tc.draw(taus_gen()));
                name.push_str(&i.to_string());
                (name, layer)
            })
            .collect();
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::create(tmp.path(), None).unwrap();
            for (name, layer) in &entries {
                store.append(name, layer.clone()).unwrap();
            }
            store.flush().unwrap();
        }
        let store: Disk<i32> = Disk::open(tmp.path(), None).unwrap();
        for (name, layer) in &entries {
            let t = layer.min_start;
            let expected = layer.at(t).copied();
            assert_eq!(store.at(name, t), expected);
        }
    }
}
