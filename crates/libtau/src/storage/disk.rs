//! Disk-persisted backend for the store.
//!
//! Binary format:
//!   Header:  MAGIC (4 bytes: "TAUZ") + VERSION (1 byte) + FLAGS (1 byte) +
//!            CRC32 (4 bytes, covers magic+version+flags)
//!   Body:    zstd-compressed payload; if FLAGS bit 0 is set, the body is
//!            AES-256-GCM encrypted before compression (compress-then-encrypt).
//!   Payload: schema section then entries.
//!     Schema:  schema_count (4 bytes, u32) + repeated DDL strings
//!              Each string: len (4 bytes, u32) + UTF-8 bytes
//!     Entries: layer_id (8 bytes, u64) + written_at_ms (8 bytes, i64) +
//!              lens_name_len (4 bytes) + lens_name +
//!              tau_count (4 bytes) + repeated taus
//!              Each tau: start (8 bytes, i64) + end (8 bytes, i64) + value (encoded)

use crate::crypto;
use crate::model::{Layer, LayerId, Tau};
use crate::storage::layers::compact_layers;
use crate::storage::store::{COMPACT_THRESHOLD, Store};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crc32fast::Hasher;

const MAGIC: &[u8] = b"TAUZ";
/// On-disk format version. The only supported version; bump on any layout
/// change once files exist in the wild.
const VERSION: u8 = 1;
const FLAG_ENCRYPTED: u8 = 0x01;
const HEADER_LEN: usize = 4 + 1 + 1 + 4; // magic + version + flags + crc32
/// Upper bound on how many elements a length-prefixed read pre-allocates. A
/// corrupted length field must not be able to trigger an oversized allocation;
/// genuine data simply grows the vec past this hint.
const MAX_PREALLOC_TAUS: usize = 1 << 16;

/// A decoded disk image: the schema DDL strings plus the per-lens layer stacks.
type DecodedImage<V> = (Vec<String>, HashMap<String, Vec<Layer<V>>>);
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
    // `len` is untrusted; read incrementally rather than pre-allocating it so a
    // corrupt length field cannot trigger an oversized allocation. Fewer than
    // `len` bytes available is a clean InvalidData error.
    let mut buf = Vec::new();
    let read = reader.take(len as u64).read_to_end(&mut buf)?;
    if read != len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated string on read",
        ));
    }
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
    let mut schema = Vec::with_capacity(count.min(MAX_PREALLOC_TAUS));
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
    /// Wall-clock write time (ms since Unix epoch).
    written_at: i64,
    lens: String,
    taus: Vec<Tau<V>>,
}

fn read_i64(reader: &mut impl Read) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

/// Write a layer entry without cloning its taus (used by [`Disk::flush`], which
/// otherwise has nothing to do with an owned [`DiskEntry`]).
fn write_entry<V: Codec, W: Write>(
    writer: &mut W,
    layer_id: LayerId,
    written_at: i64,
    lens: &str,
    taus: &[Tau<V>],
) -> io::Result<()> {
    writer.write_all(&layer_id.to_le_bytes())?;
    writer.write_all(&written_at.to_le_bytes())?;
    write_str(writer, lens)?;
    writer.write_all(&(taus.len() as u32).to_le_bytes())?;
    for tau in taus {
        writer.write_all(&tau.start().to_le_bytes())?;
        writer.write_all(&tau.end().to_le_bytes())?;
        tau.value.write_encoded(writer)?;
    }
    Ok(())
}

impl<V: Codec> DiskEntry<V> {
    fn read(reader: &mut impl Read) -> io::Result<Self> {
        let layer_id = read_i64(reader)? as u64;
        let written_at = read_i64(reader)?;
        let lens = read_str(reader)?;

        let mut count_buf = [0u8; 4];
        reader.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;

        // `count` comes from a possibly-corrupted file, so never pre-allocate
        // more than a sane bound — a bogus count would otherwise abort the
        // process on an oversized allocation. Genuine layers just grow the vec.
        let mut taus = Vec::with_capacity(count.min(MAX_PREALLOC_TAUS));
        for _ in 0..count {
            let start = read_i64(reader)?;
            let end = read_i64(reader)?;
            let value = V::read_encoded(reader)?;
            // Corruption can decode into a degenerate/inverted interval; reject
            // it cleanly instead of panicking in `Tau::new`.
            let tau = Tau::try_new(start, end, value).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid tau interval on read: start {start} >= end {end}"),
                )
            })?;
            taus.push(tau);
        }

        Ok(DiskEntry {
            layer_id,
            written_at,
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
    lenses: HashMap<String, Arc<[Layer<V>]>>,
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
        let (schema, decoded) = if path.exists() {
            let file = File::open(&path)?;
            let mut reader = BufReader::new(file);
            Self::decode_image(&mut reader, key)?
        } else {
            (Vec::new(), HashMap::new())
        };
        // Freeze each decoded stack into a shared `Arc<[Layer]>` for O(1) reads.
        let lenses: HashMap<String, Arc<[Layer<V>]>> = decoded
            .into_iter()
            .map(|(name, layers)| (name, Arc::from(layers)))
            .collect();

        Ok(Self {
            path,
            lenses,
            schema,
            compact_threshold: COMPACT_THRESHOLD,
            key,
            compression_level: DEFAULT_ZSTD_LEVEL,
        })
    }

    /// Decode a disk image (header, optional decryption, zstd body, schema, and
    /// layer entries) from a reader. Shared by [`Disk::open`] and
    /// [`Disk::decode_image_bytes`] so the exact production decode path is what
    /// fuzzing exercises. Returns a clean [`io::Error`] on malformed input — it
    /// must never panic.
    fn decode_image<R: Read>(reader: &mut R, key: Option<[u8; 32]>) -> io::Result<DecodedImage<V>> {
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
        if version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported version: expected {VERSION}, got {version}"),
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
        Self::decode_payload(&mut cursor)
    }

    /// Decode the *decompressed* payload — the schema section followed by the
    /// layer entries — from a reader. Split out from [`Disk::decode_image`] so
    /// the vulnerable byte-parsing logic (`read_schema`, `DiskEntry::read`,
    /// `read_str`) can be fuzzed directly, without a blind fuzzer first having to
    /// satisfy the CRC header and produce valid zstd.
    fn decode_payload<R: Read>(reader: &mut R) -> io::Result<DecodedImage<V>> {
        let schema = read_schema(reader)?;
        let mut lenses: HashMap<String, Vec<Layer<V>>> = HashMap::new();
        loop {
            match DiskEntry::read(reader) {
                Ok(entry) => {
                    // Corruption can decode overlapping taus; reject the layer
                    // cleanly rather than panicking in `Layer::new_at`.
                    let layer = Layer::try_new_at(entry.layer_id, entry.taus, entry.written_at)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "overlapping taus in decoded layer",
                            )
                        })?;
                    lenses.entry(entry.lens.clone()).or_default().push(layer);
                }
                Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
        }
        Ok((schema, lenses))
    }

    /// Decode a full disk image (header, optional decryption, zstd, payload) from
    /// a byte slice. In-memory entry point for the binary-format fuzz target;
    /// runs the same path as [`Disk::open`] without touching the filesystem and
    /// returns a clean error rather than panicking on malformed input.
    pub fn decode_image_bytes(bytes: &[u8], key: Option<[u8; 32]>) -> io::Result<()> {
        let mut cursor = Cursor::new(bytes);
        Self::decode_image(&mut cursor, key).map(|_| ())
    }

    /// Decode an already-decompressed payload (schema + entries) from a byte
    /// slice. This is the high-signal fuzz entry point: it reaches the interval
    /// and length-prefix parsing directly, bypassing the CRC/zstd envelope a
    /// blind fuzzer would otherwise be stuck on. Must never panic.
    pub fn decode_payload_bytes(bytes: &[u8]) -> io::Result<()> {
        let mut cursor = Cursor::new(bytes);
        Self::decode_payload(&mut cursor).map(|_| ())
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
            for layer in layers.iter() {
                write_entry(
                    &mut entries,
                    layer.id,
                    layer.written_at,
                    lens_name,
                    &layer.taus,
                )?;
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
            crypto::encrypt(enc_key, &compressed)?
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
        // RCU: copy the stack, mutate, swap the Arc in (see `InMemory::append`).
        let mut layers: Vec<Layer<V>> = self
            .lenses
            .get(lens)
            .map(|a| a.to_vec())
            .unwrap_or_default();
        let before = layers.len();
        layers.push(layer);
        let mut did_compact = false;
        if layers.len() > self.compact_threshold {
            compact_layers(&mut layers);
            did_compact = layers.len() < before + 1;
        }
        self.lenses.insert(lens.to_string(), Arc::from(layers));
        // Durability for the new layer comes from the WAL that `Database`
        // pairs with every disk-backed store; the full-file rewrite here is
        // reserved for `checkpoint_flush`, called on compaction/checkpoint.
        Ok(did_compact)
    }

    fn drop_lens(&mut self, lens: &str) {
        self.lenses.remove(lens);
    }

    fn layers(&self, lens: &str) -> Option<Arc<[Layer<V>]>> {
        self.lenses.get(lens).cloned()
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

    fn checkpoint_flush(&self) -> io::Result<bool> {
        self.flush()?;
        Ok(true)
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

    /// Hand-encode one `DiskEntry` body (the post-decompression layout) with a
    /// single tau, so corruption can be modelled without touching the
    /// compress/encrypt envelope.
    fn encode_entry(lens: &str, start: i64, end: i64, value: i64) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes()); // layer_id
        buf.extend_from_slice(&0i64.to_le_bytes()); // written_at
        buf.extend_from_slice(&(lens.len() as u32).to_le_bytes());
        buf.extend_from_slice(lens.as_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // tau count
        buf.extend_from_slice(&start.to_le_bytes());
        buf.extend_from_slice(&end.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
        buf
    }

    #[test]
    fn disk_entry_read_round_trips_valid_interval() {
        let bytes = encode_entry("temp", 0, 10, 42);
        let mut cur = Cursor::new(bytes);
        let entry = DiskEntry::<i64>::read(&mut cur).expect("valid entry");
        assert_eq!(entry.taus.len(), 1);
        assert_eq!((entry.taus[0].start(), entry.taus[0].end()), (0, 10));
    }

    #[test]
    fn disk_entry_read_rejects_inverted_interval_without_panicking() {
        // Corruption can decode into start >= end; the reader must return a
        // clean InvalidData error rather than panicking in `Tau::new`.
        let bytes = encode_entry("temp", 10, 5, 42);
        let mut cur = Cursor::new(bytes);
        let err = DiskEntry::<i64>::read(&mut cur).expect_err("inverted interval");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_str_rejects_oversized_length_without_oom() {
        // A corrupt length prefix far exceeding the available bytes must be a
        // clean error, not an oversized allocation.
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"only a few bytes");
        let mut cur = Cursor::new(bytes);
        let err = read_str(&mut cur).expect_err("truncated string");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_image_bytes_round_trips_a_real_image() {
        let tmp = NamedTempFile::new().unwrap();
        let layer = Layer::new_at(1, vec![Tau::new(0, 10, 7i64)], 1_700_000_000_000);
        let mut store = Disk::<i64>::create(tmp.path(), None).unwrap();
        store.append("temp", layer).unwrap();
        store.flush().unwrap();

        let bytes = fs::read(tmp.path()).unwrap();
        // The exact bytes that `Disk::open` would read must decode cleanly.
        Disk::<i64>::decode_image_bytes(&bytes, None).expect("valid image decodes");
    }

    #[test]
    fn decode_image_bytes_rejects_arbitrary_input_without_panicking() {
        // The fuzz entry point's contract: any byte string is a clean Ok/Err,
        // never a panic.
        for bad in [
            &b""[..],
            &b"TAUZ"[..],
            &b"not a tau file at all"[..],
            &[0xff; 64][..],
        ] {
            let _ = Disk::<i64>::decode_image_bytes(bad, None);
        }
    }

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
    fn written_at_survives_reopen() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut store = Disk::<i64>::create(tmp.path(), None).unwrap();
            store
                .append(
                    "x",
                    Layer::new_at(1, vec![Tau::new(0, 10, 7)], 1_717_000_000_123),
                )
                .unwrap();
            store.flush().unwrap();
        }
        let store: Disk<i64> = Disk::open(tmp.path(), None).unwrap();
        assert_eq!(store.layers("x").unwrap()[0].written_at, 1_717_000_000_123);
    }

    /// Only the current format version opens; any other version byte (with a
    /// valid header checksum) is rejected up front.
    #[test]
    fn unknown_version_is_rejected() {
        for bad_version in [0u8, VERSION + 1] {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(MAGIC);
            bytes.push(bad_version);
            bytes.push(0u8); // flags: unencrypted
            let crc = checksum(&bytes);
            bytes.extend_from_slice(&crc.to_le_bytes());

            let tmp = NamedTempFile::new().unwrap();
            fs::write(tmp.path(), &bytes).unwrap();
            let result: io::Result<Disk<i64>> = Disk::open(tmp.path(), None);
            assert!(result.is_err(), "version {bad_version} must be rejected");
        }
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
