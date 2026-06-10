//! Write-ahead log for durable append semantics.
//!
//! # Design
//!
//! The WAL sits between `Database` and `Store`:
//!
//! ```text
//! Database::append
//!     └─► Wal::append   (fsync to disk first)
//!     └─► Store::append (then update in-memory state)
//! ```
//!
//! On startup, `Wal::replay` feeds every persisted entry back into a
//! fresh `Store` so the in-memory view is reconstructed from durable
//! state before any new writes are accepted.
//!
//! # Serialisation
//!
//! Each WAL entry is one line: checksum header + serialized payload.
//!
//! ```text
//! <crc32> <layer_id> <lens_name> <start0>:<end0>:<value0> <start1>:<end1>:<value1> …
//! ```
//!
//! The CRC32 covers everything after the checksum field itself.
//! Values are encoded/decoded through the [`Codec`] trait so the WAL
//! has no dependency on `serde` and callers supply their own wire format
//! (e.g. plain integers, base64, JSON fragments).

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use crc32fast::Hasher;

use crate::crypto;
use crate::model::{Layer, LayerId, Tau, Timestamp};
use crate::storage::Store;
#[cfg(test)]
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, instrument, warn};

/// Encode a single value `V` to/from a string token with no whitespace or
/// colons.  Implementations ship for the primitive numeric types below.
pub trait Codec: Sized {
    fn encode(&self) -> String;
    fn decode(s: &str) -> Option<Self>;
    /// Write the encoded form directly into `buf` without an intermediate `String` allocation.
    /// The default delegates to `encode()`; override for zero-allocation hot paths.
    fn encode_into(&self, buf: &mut String) {
        buf.push_str(&self.encode());
    }
}

macro_rules! impl_codec_display_parse {
    ($($t:ty),+) => {
        $(impl Codec for $t {
            fn encode(&self) -> String { self.to_string() }
            fn decode(s: &str) -> Option<Self> { s.parse().ok() }
            fn encode_into(&self, buf: &mut String) {
                use std::fmt::Write as _;
                _ = write!(buf, "{}", self);
            }
        })+
    };
}

impl_codec_display_parse!(
    i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, f32, f64, bool
);

/// Compute CRC32 of a string slice.
fn crc32(data: &str) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data.as_bytes());
    hasher.finalize()
}

fn write_u32(w: &mut impl io::Write, n: u32) -> io::Result<()> {
    let mut buf = itoa::Buffer::new();
    w.write_all(buf.format(n).as_bytes())
}

/// Write one serialised data entry, encrypting (`E:` + base64) when a key is set.
fn write_data_line<V: Codec>(
    w: &mut impl Write,
    key: &Option<[u8; 32]>,
    entry: &WalEntry<V>,
) -> io::Result<()> {
    if let Some(key) = key {
        let blob = crypto::encrypt(key, entry.serialise().as_bytes());
        writeln!(w, "E:{}", B64.encode(&blob))
    } else {
        writeln!(w, "{}", entry.serialise())
    }
}

/// One serialised append record.
#[derive(Debug, Clone, PartialEq)]
pub struct WalEntry<V> {
    pub layer_id: LayerId,
    /// Wall-clock milliseconds since Unix epoch when this layer was written.
    pub written_at: i64,
    pub lens: String,
    pub taus: Vec<(Timestamp, Timestamp, V)>,
}

impl<V: Clone> WalEntry<V> {
    /// Snapshot a layer as a WAL entry (Arc-backed taus make the clone cheap).
    pub fn from_layer(lens: &str, layer: &Layer<V>) -> Self {
        Self {
            layer_id: layer.id,
            written_at: layer.written_at,
            lens: lens.to_string(),
            taus: layer
                .taus
                .iter()
                .map(|t| (t.start, t.end, t.value.clone()))
                .collect(),
        }
    }
}

impl<V: Codec> WalEntry<V> {
    /// Serialise to the wire format with checksum prefix.
    /// Format: `<crc32> <layer_id> <written_at_ms> <lens_name> [<taus>]`
    pub fn serialise(&self) -> String {
        let taus_str = self
            .taus
            .iter()
            .map(|(s, e, v)| format!("{}:{}:{}", s, e, v.encode()))
            .collect::<Vec<_>>()
            .join(" ");
        let payload = if self.taus.is_empty() {
            format!("{} {} {}", self.layer_id, self.written_at, self.lens)
        } else {
            format!(
                "{} {} {} {}",
                self.layer_id, self.written_at, self.lens, taus_str
            )
        };
        let checksum = crc32(&payload);
        format!("{} {}", checksum, payload)
    }

    /// Deserialise from one line of the log file, verifying checksum.
    /// Format: `<crc32> <layer_id> <written_at_ms> <lens_name> [<taus>]`.
    pub fn deserialise(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        let (checksum_str, payload) = line.split_once(' ')?;

        let checksum: u32 = checksum_str.parse().ok()?;
        let expected = crc32(payload);
        if checksum != expected {
            warn!(
                expected = checksum,
                actual = expected,
                "WAL entry checksum mismatch, discarding"
            );
            return None;
        }

        let mut tokens = payload.splitn(4, ' ');
        let layer_id: LayerId = tokens.next()?.parse().ok()?;
        let written_at: i64 = tokens.next()?.parse().ok()?;
        let lens = tokens.next()?.to_string();
        let rest_str = tokens.next().unwrap_or("");

        let taus = if rest_str.is_empty() {
            vec![]
        } else {
            rest_str
                .split(' ')
                .map(|tok| {
                    let mut parts = tok.splitn(3, ':');
                    let s: Timestamp = parts.next()?.parse().ok()?;
                    let e: Timestamp = parts.next()?.parse().ok()?;
                    let v = V::decode(parts.next()?)?;
                    Some((s, e, v))
                })
                .collect::<Option<Vec<_>>>()?
        };

        Some(WalEntry {
            layer_id,
            written_at,
            lens,
            taus,
        })
    }
}

/// Decrypt a WAL entry (data or schema). `label` prefixes all warning messages.
fn decrypt_wal_entry(rest: &str, key: &Option<[u8; 32]>, label: &str) -> Option<String> {
    let key = match key {
        Some(k) => k,
        None => {
            warn!("{label} found but no key configured, skipping");
            return None;
        }
    };
    let blob = match B64.decode(rest) {
        Ok(b) => b,
        Err(_) => {
            warn!("{label} base64 decode failed, skipping");
            return None;
        }
    };
    match crypto::decrypt(key, &blob) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Some(s),
            Err(_) => {
                warn!("{label} decrypted but not valid UTF-8, skipping");
                None
            }
        },
        Err(_) => {
            warn!("{label} decryption failed, skipping");
            None
        }
    }
}

/// Validate the checksum in `inner` (`"<crc32> <stmt>"`) and push `stmt` into `out` if valid.
fn validate_schema_checksum(inner: &str, out: &mut Vec<String>) {
    if let Some((ck_str, stmt)) = inner.split_once(' ') {
        match ck_str.parse::<u32>() {
            Ok(ck) if ck == crc32(stmt) => out.push(stmt.to_string()),
            Ok(_) => warn!("schema WAL entry checksum mismatch, discarding"),
            Err(_) => warn!("schema WAL entry malformed checksum, discarding"),
        }
    } else {
        warn!("schema WAL entry missing checksum, discarding");
    }
}

/// Append-only write-ahead log backed by a single flat file.
pub struct Wal {
    writer: BufWriter<File>,
    path: PathBuf,
    key: Option<[u8; 32]>,
    /// When `false`, `append` skips the per-record `flush + sync_data` and
    /// callers must call [`Wal::sync`] explicitly (or rely on drop / process
    /// exit) to flush.  Defaults to `true` so existing call sites are
    /// unchanged.
    fsync_each: bool,
    /// Reusable buffer for serialising a record before checksum + write.
    /// Kept on the struct so the hot path makes no per-record allocations.
    scratch: String,
    /// When set, `needs_rotation()` returns `true` once the WAL file on disk
    /// reaches or exceeds this many bytes.  The caller (Database) is
    /// responsible for acting on the signal by calling `checkpoint()`.
    max_bytes: Option<u64>,
}

impl Wal {
    /// Open (or create) the WAL at `path`. Pass `Some(key)` to enable AES-256-GCM
    /// encryption of every entry. An existing unencrypted WAL remains readable when
    /// `key` is `None`; encrypted entries require the same key used to write them.
    #[instrument(fields(path = %path.as_ref().display()))]
    pub fn open(path: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        debug!("opening WAL file");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Larger BufWriter capacity reduces flush frequency on the fsync=false
        // path; 256 KiB amortises well at typical record sizes.
        Ok(Self {
            writer: BufWriter::with_capacity(256 * 1024, file),
            path,
            key,
            fsync_each: true,
            scratch: String::with_capacity(256),
            max_bytes: None,
        })
    }

    /// Toggle per-record `fsync`.  When set to `false`, [`Wal::append`] and
    /// [`Wal::append_schema`] skip the synchronous flush + `sync_data` and the
    /// caller assumes responsibility for durability (call [`Wal::sync`] before
    /// considering data committed).  Defaults to `true`.
    pub fn set_fsync_each(&mut self, on: bool) {
        self.fsync_each = on;
    }

    /// Set a soft file-size cap in bytes.  Once the WAL file on disk reaches
    /// or exceeds `bytes`, [`Wal::needs_rotation`] returns `true`.  The
    /// caller is responsible for responding by calling `Database::checkpoint`,
    /// which rewrites the WAL to contain only the current live layers.
    pub fn set_max_bytes(&mut self, bytes: u64) {
        self.max_bytes = Some(bytes);
    }

    /// Return the current on-disk size of the WAL file in bytes, or `0` if the
    /// file cannot be stat-ed.  Cheap: a single `fstat` syscall.
    pub fn size_bytes(&self) -> u64 {
        fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// Returns `true` when a max-bytes limit is configured and the WAL file on
    /// disk has reached or exceeded it.  Cheap: a single `fstat` syscall.
    pub fn needs_rotation(&self) -> bool {
        let Some(max) = self.max_bytes else {
            return false;
        };
        fs::metadata(&self.path)
            .map(|m| m.len() >= max)
            .unwrap_or(false)
    }

    /// Explicitly flush the buffered writer and `sync_data` the underlying
    /// file.  Use this after a batch of [`Wal::append`] calls when
    /// [`Wal::set_fsync_each`] is disabled.
    pub fn sync(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    fn maybe_sync(&mut self) -> io::Result<()> {
        if self.fsync_each { self.sync() } else { Ok(()) }
    }

    /// Write one entry to disk, flush before returning.
    ///
    /// If a key is configured, the serialised entry is encrypted with AES-256-GCM
    /// (random 12-byte nonce per entry) and stored base64-encoded with an `E:` prefix.
    pub fn append<V: Codec>(&mut self, entry: &WalEntry<V>) -> io::Result<()> {
        debug!(
            lens = %entry.lens,
            layer_id = entry.layer_id,
            tau_count = entry.taus.len(),
            "writing WAL entry"
        );
        write_data_line(&mut self.writer, &self.key, entry)?;
        self.maybe_sync()
    }

    /// Fast path: serialise a [`Layer`] directly into the WAL without
    /// constructing an intermediate `WalEntry` or its temporary `Vec`s.
    /// Uses a reusable scratch buffer for the payload so the hot path makes
    /// **zero** allocations beyond integer formatting.
    pub fn append_layer<V: Codec + Clone>(
        &mut self,
        lens: &str,
        layer: &crate::model::Layer<V>,
    ) -> io::Result<()> {
        use std::fmt::Write as _;

        // Encrypted path keeps the original serialise() route - encryption
        // dominates cost and the buffer allocation noise is negligible.
        if self.key.is_some() {
            return self.append(&WalEntry::from_layer(lens, layer));
        }

        self.scratch.clear();
        _ = write!(
            &mut self.scratch,
            "{} {} {}",
            layer.id, layer.written_at, lens
        );
        for tau in layer.taus.iter() {
            _ = write!(&mut self.scratch, " {}:{}:", tau.start, tau.end);
            tau.value.encode_into(&mut self.scratch);
        }
        let checksum = crc32(&self.scratch);
        // Write without writeln! to avoid the internal String allocation that
        // io::Write::write_fmt incurs when formatting multiple values in one call.
        write_u32(&mut self.writer, checksum)?;
        self.writer.write_all(b" ")?;
        self.writer.write_all(self.scratch.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.maybe_sync()
    }

    /// Replay every persisted entry into `store` in write order.
    ///
    /// Lines prefixed with `E:` are base64-decoded and decrypted before parsing.
    /// Corrupt or undecryptable entries are skipped with a warning.
    /// Call this once during startup, before accepting new writes.
    #[instrument(skip(self, store), fields(path = %self.path.display()))]
    pub fn replay<V>(&self, store: &mut dyn Store<V>) -> io::Result<()>
    where
        V: Codec + Clone + Send + Sync + 'static,
    {
        debug!("starting WAL replay");
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut count = 0usize;
        let mut skipped = 0usize;

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let plaintext: String = if let Some(rest) = line.strip_prefix("E:") {
                match decrypt_wal_entry(rest, &self.key, "encrypted WAL entry") {
                    Some(s) => s,
                    None => {
                        skipped += 1;
                        continue;
                    }
                }
            } else {
                line.to_string()
            };

            if let Some(entry) = WalEntry::<V>::deserialise(&plaintext) {
                debug!(
                    lens = %entry.lens,
                    layer_id = entry.layer_id,
                    tau_count = entry.taus.len(),
                    "replaying WAL entry"
                );

                let taus: Vec<Tau<V>> = entry
                    .taus
                    .into_iter()
                    .map(|(s, e, v)| Tau::new(s, e, v))
                    .collect();

                if let Err(e) = store.append(
                    &entry.lens,
                    Layer::new_at(entry.layer_id, taus, entry.written_at),
                ) {
                    warn!(error = %e, "WAL replay: store append failed, skipping");
                    skipped += 1;
                    continue;
                }
                count += 1;
            } else {
                skipped += 1;
            }
        }

        debug!(
            entries_replayed = count,
            entries_skipped = skipped,
            "WAL replay complete"
        );
        Ok(())
    }

    /// Append a schema DDL statement (e.g. `CREATE LENS temp int`) to the log.
    ///
    /// Plaintext format: `S:<crc32> <stmt_text>`
    /// Encrypted format: `SE:<base64>` where the plaintext is `<crc32> <stmt_text>`
    pub fn append_schema(&mut self, stmt_text: &str) -> io::Result<()> {
        let ck = crc32(stmt_text);
        let inner = format!("{} {}", ck, stmt_text);
        if let Some(key) = &self.key {
            let blob = crypto::encrypt(key, inner.as_bytes());
            writeln!(self.writer, "SE:{}", B64.encode(&blob))?;
        } else {
            writeln!(self.writer, "S:{}", inner)?;
        }
        self.maybe_sync()
    }

    /// Return the raw `S:` / `SE:` lines as they appear in the WAL file.
    ///
    /// Used by `Wal::rewrite` so that schema lines are preserved verbatim
    /// during WAL rotation without needing to re-encrypt them.
    pub fn raw_schema_lines(&self) -> io::Result<Vec<String>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut lines = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.starts_with("S:") || trimmed.starts_with("SE:") {
                lines.push(line);
            }
        }
        Ok(lines)
    }

    /// Return the decoded schema DDL statements stored in the WAL.
    ///
    /// Each returned string is a raw statement text such as
    /// `CREATE LENS temp int` or `DERIVE LENS fast AS avg(slow, -3600, 0)`.
    /// Corrupt or undecryptable entries are skipped with a warning.
    pub fn replay_schemas(&self) -> io::Result<Vec<String>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut stmts = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();

            let inner: String = if let Some(rest) = trimmed.strip_prefix("SE:") {
                match decrypt_wal_entry(rest, &self.key, "encrypted schema WAL entry") {
                    Some(s) => s,
                    None => continue,
                }
            } else if let Some(rest) = trimmed.strip_prefix("S:") {
                rest.to_string()
            } else {
                continue;
            };

            validate_schema_checksum(&inner, &mut stmts);
        }
        Ok(stmts)
    }

    /// Rotate the WAL: archive the current file to `<path>.<unix_ms>`, then
    /// write a fresh file containing `raw_schema_lines` followed by `entries`.
    ///
    /// The archive is a permanent on-disk copy of the pre-rotation state and
    /// can be used for point-in-time recovery.  The live path always holds only
    /// the post-rotation (compacted) content.  Crash-safe: uses a `.tmp` sibling
    /// and atomic rename before archiving the old file.
    pub fn rotate<V: Codec>(
        &mut self,
        raw_schema_lines: &[String],
        entries: &[WalEntry<V>],
    ) -> io::Result<()> {
        // Flush any buffered writes before archiving.
        self.writer.flush()?;

        // Write fresh content to a tmp file first for crash safety.
        let tmp = PathBuf::from(format!("{}.tmp", self.path.display()));
        {
            let tmp_file = File::create(&tmp)?;
            let mut writer = BufWriter::new(tmp_file);
            for line in raw_schema_lines {
                writeln!(writer, "{}", line)?;
            }
            for entry in entries {
                write_data_line(&mut writer, &self.key, entry)?;
            }
            writer.flush()?;
            writer.get_ref().sync_data()?;
        }

        // Archive the current live file.
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let archive = PathBuf::from(format!("{}.{ts}", self.path.display()));
        fs::rename(&self.path, &archive)?;

        // Promote the tmp file to the live path.
        fs::rename(&tmp, &self.path)?;

        // Reopen the writer in append mode on the new live file.
        let file = OpenOptions::new().append(true).open(&self.path)?;
        self.writer = BufWriter::with_capacity(256 * 1024, file);
        Ok(())
    }

    /// Return the set of layer IDs present as data entries in the WAL.
    ///
    /// Used internally to verify WAL rotation in tests.
    #[cfg(test)]
    pub fn data_layer_ids(&self) -> io::Result<HashSet<LayerId>> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut ids = HashSet::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("S:") || trimmed.starts_with("SE:") {
                continue;
            }
            // Data line: "<crc32> <layer_id> <lens> [taus...]"
            // We only need layer_id; skip checksum verification here.
            let mut parts = trimmed.splitn(3, ' ');
            let _ = parts.next();
            if let Some(id_str) = parts.next()
                && let Ok(id) = id_str.parse::<LayerId>()
            {
                ids.insert(id);
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory::InMemory;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;

    /// Generator over lens names that survive the line-oriented WAL format.
    fn lens_name_gen() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9_]{0,8}").fullmatch(true)
    }

    /// Generator over `WalEntry<i64>` with sorted non-overlapping taus.
    fn entry_gen_i64() -> impl Generator<WalEntry<i64>> {
        lens_name_gen().flat_map(|name| {
            gs::integers::<u64>()
                .min_value(1)
                .max_value(10_000)
                .flat_map(move |layer_id| {
                    let name = name.clone();
                    gs::vecs(
                        gs::integers::<i64>()
                            .min_value(1)
                            .max_value(50)
                            .flat_map(|width| {
                                gs::integers::<i64>().min_value(0).max_value(20).flat_map(
                                    move |gap| gs::integers::<i64>().map(move |v| (width, gap, v)),
                                )
                            }),
                    )
                    .max_size(8)
                    .map(move |specs| {
                        let mut taus = Vec::with_capacity(specs.len());
                        let mut cursor: i64 = 0;
                        for (width, gap, v) in specs {
                            let s = cursor + gap;
                            let e = s + width;
                            taus.push((s, e, v));
                            cursor = e;
                        }
                        WalEntry::<i64> {
                            layer_id,
                            written_at: 0,
                            lens: name.clone(),
                            taus,
                        }
                    })
                })
        })
    }

    #[hegel::test]
    fn pbt_entry_serialise_deserialise_roundtrips_i64(tc: TestCase) {
        let entry = tc.draw(entry_gen_i64());
        let line = entry.serialise();
        let decoded = WalEntry::<i64>::deserialise(&line).expect("decode own serialisation");
        assert_eq!(decoded, entry);
    }

    #[hegel::test]
    fn pbt_entry_serialise_starts_with_checksum_digits(tc: TestCase) {
        let entry = tc.draw(entry_gen_i64());
        let line = entry.serialise();
        assert!(
            line.chars().next().unwrap().is_ascii_digit(),
            "checksum must be a leading decimal integer: {line:?}"
        );
    }

    #[hegel::test]
    fn pbt_entry_deserialise_never_panics_on_arbitrary_text(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(256));
        // Both i64 and bool decoders must tolerate any text without panicking.
        let _ = WalEntry::<i64>::deserialise(&s);
        let _ = WalEntry::<bool>::deserialise(&s);
    }

    #[hegel::test]
    fn pbt_entry_deserialise_rejects_checksum_corruption(tc: TestCase) {
        let entry = tc.draw(entry_gen_i64());
        let line = entry.serialise();
        // Pick a non-zero corruption to bump the leading digit.  Any single
        // character flip in the checksum must invalidate the line.
        let delta = tc.draw(gs::integers::<u32>().min_value(1).max_value(8));
        let first_byte = line.as_bytes()[0];
        let corrupted_first = ((first_byte - b'0' + delta as u8) % 10) + b'0';
        if corrupted_first == first_byte {
            return; // generator picked an effective no-op
        }
        let mut corrupted = String::from(corrupted_first as char);
        corrupted.push_str(&line[1..]);
        assert!(WalEntry::<i64>::deserialise(&corrupted).is_none());
    }

    #[hegel::test]
    fn pbt_codec_i64_roundtrips_every_value(tc: TestCase) {
        let v = tc.draw(gs::integers::<i64>());
        assert_eq!(i64::decode(&v.encode()), Some(v));
    }

    #[hegel::test]
    fn pbt_codec_f64_roundtrips_finite_values(tc: TestCase) {
        let v = tc.draw(gs::floats::<f64>().filter(|f| f.is_finite()));
        let decoded = f64::decode(&v.encode()).unwrap();
        assert!((decoded - v).abs() <= f64::EPSILON * v.abs().max(1.0));
    }

    #[hegel::test]
    fn pbt_codec_bool_roundtrips(tc: TestCase) {
        let v = tc.draw(gs::booleans());
        assert_eq!(bool::decode(&v.encode()), Some(v));
    }

    fn check_encode_into<V: Codec>(v: &V) {
        let mut buf = String::new();
        v.encode_into(&mut buf);
        assert_eq!(buf, v.encode());
    }

    #[hegel::test]
    fn pbt_codec_i64_encode_into_matches_encode(tc: TestCase) {
        check_encode_into(&tc.draw(gs::integers::<i64>()));
    }

    #[hegel::test]
    fn pbt_codec_f64_encode_into_matches_encode(tc: TestCase) {
        check_encode_into(&tc.draw(gs::floats::<f64>().filter(|f| f.is_finite())));
    }

    #[hegel::test]
    fn pbt_codec_bool_encode_into_matches_encode(tc: TestCase) {
        check_encode_into(&tc.draw(gs::booleans()));
    }

    #[test]
    fn test_write_u32() {
        let cases = vec![
            (0u32, "0"),
            (7u32, "7"),
            (42u32, "42"),
            (1234567890u32, "1234567890"),
            (u32::MAX, "4294967295"),
        ];
        for (n, expected) in cases {
            let mut buf = Vec::new();
            super::write_u32(&mut buf, n).unwrap();
            assert_eq!(String::from_utf8(buf).unwrap(), expected);
        }
    }

    #[hegel::test]
    fn pbt_codec_i64_decode_returns_none_on_unparseable(tc: TestCase) {
        let s = tc
            .draw(gs::text().max_size(32))
            .replace(|c: char| c.is_ascii_digit() || c == '-', "x");
        if s.is_empty() {
            return;
        }
        assert_eq!(i64::decode(&s), None);
    }

    /// Append a sequence of entries to a WAL (encrypted or plain), then
    /// replay into a fresh `InMemory` store.  For any timestamp probe, the
    /// resulting `at` must agree with what we'd get from running the same
    /// entries through `InMemory` directly.
    fn wal_replay_matches_inmemory(tc: TestCase, key: Option<[u8; 32]>) {
        let entries: Vec<WalEntry<i64>> = (0..tc
            .draw(gs::integers::<usize>().min_value(0).max_value(5)))
            .map(|_| tc.draw(entry_gen_i64()))
            .collect();
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), key).unwrap();
            for e in &entries {
                wal.append(e).unwrap();
            }
        }
        let mut replayed: InMemory<i64> = InMemory::with_threshold(1024);
        Wal::open(tmp.path(), key)
            .unwrap()
            .replay(&mut replayed)
            .unwrap();

        let mut reference: InMemory<i64> = InMemory::with_threshold(1024);
        for e in &entries {
            if e.taus.is_empty() {
                continue;
            }
            let layer = Layer::new(
                e.layer_id,
                e.taus
                    .iter()
                    .map(|(s, e, v)| Tau::new(*s, *e, *v))
                    .collect(),
            );
            reference.append(&e.lens, layer).unwrap();
        }

        let probe = tc.draw(gs::integers::<i64>().min_value(-100).max_value(2000));
        for e in &entries {
            assert_eq!(replayed.at(&e.lens, probe), reference.at(&e.lens, probe));
        }
    }

    #[hegel::test]
    fn pbt_wal_unencrypted_replay_matches_inmemory(tc: TestCase) {
        wal_replay_matches_inmemory(tc, None);
    }

    #[hegel::test]
    fn pbt_wal_encrypted_replay_matches_inmemory(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        wal_replay_matches_inmemory(tc, Some(key));
    }

    #[hegel::test]
    fn pbt_wal_encrypted_lines_start_with_e_prefix(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let entry = tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty()));
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), Some(key)).unwrap();
            wal.append(&entry).unwrap();
        }
        let raw = fs::read_to_string(tmp.path()).unwrap();
        let first = raw.lines().next().unwrap();
        assert!(first.starts_with("E:"), "encrypted line must start with E:");
    }

    #[hegel::test]
    fn pbt_wal_encrypted_replay_without_key_skips_entries(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let entry = tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty()));
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), Some(key)).unwrap();
            wal.append(&entry).unwrap();
        }
        let mut store: InMemory<i64> = InMemory::with_threshold(1024);
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();
        // The lens may not even exist after a no-key replay.
        let probe = entry.taus.first().map(|(s, _, _)| *s).unwrap_or(0);
        assert_eq!(store.at(&entry.lens, probe), None);
    }

    #[test]
    fn wal_open_creates_file_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.wal");
        assert!(!path.exists());
        Wal::open(&path, None).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn wal_replay_on_empty_file_is_a_noop() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();
        assert_eq!(store.at("anything", 0), None);
    }

    #[hegel::test]
    fn pbt_wal_skips_corrupted_entries_during_replay(tc: TestCase) {
        let good_a = tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty()));
        let good_b = tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty()));
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut f = OpenOptions::new().write(true).open(tmp.path()).unwrap();
            // Valid -> corrupted -> valid: the middle is skipped, the others replay.
            writeln!(f, "{}", good_a.serialise()).unwrap();
            writeln!(f, "999999 1 corrupt 0:10:42").unwrap();
            writeln!(f, "{}", good_b.serialise()).unwrap();
        }
        let mut store: InMemory<i64> = InMemory::with_threshold(1024);
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();

        // Reference: the same two good entries replayed into a fresh store
        // bypassing the WAL.  Compare at any timestamp - if they share a lens,
        // newest-wins applies; if not, both are visible.
        let mut reference: InMemory<i64> = InMemory::with_threshold(1024);
        for e in [&good_a, &good_b] {
            let layer = Layer::new(
                e.layer_id,
                e.taus
                    .iter()
                    .map(|(s, ee, v)| Tau::new(*s, *ee, *v))
                    .collect(),
            );
            reference.append(&e.lens, layer).unwrap();
        }
        for e in [&good_a, &good_b] {
            if let Some((s, _, _)) = e.taus.first() {
                assert_eq!(store.at(&e.lens, *s), reference.at(&e.lens, *s));
            }
        }
        // The corrupt line claims lens "corrupt" - it must not appear.
        assert_eq!(store.at("corrupt", 0), None);
    }

    #[hegel::test]
    fn pbt_wal_schema_roundtrips_unencrypted(tc: TestCase) {
        let stmts = tc.draw(
            gs::vecs(
                gs::from_regex(r"CREATE LENS [a-z]+ (int|float|str|bool|bytes)").fullmatch(true),
            )
            .max_size(5),
        );
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), None).unwrap();
            for s in &stmts {
                wal.append_schema(s).unwrap();
            }
        }
        let wal = Wal::open(tmp.path(), None).unwrap();
        assert_eq!(wal.replay_schemas().unwrap(), stmts);
    }

    #[hegel::test]
    fn pbt_wal_schema_roundtrips_encrypted(tc: TestCase) {
        let key_bytes = tc.draw(gs::vecs(gs::integers::<u8>()).min_size(32).max_size(32));
        let stmts = tc.draw(
            gs::vecs(
                gs::from_regex(r"CREATE LENS [a-z]+ (int|float|str|bool|bytes)").fullmatch(true),
            )
            .min_size(1)
            .max_size(3),
        );
        let mut key = [0u8; 32];
        key.copy_from_slice(&key_bytes);
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), Some(key)).unwrap();
            for s in &stmts {
                wal.append_schema(s).unwrap();
            }
        }
        let raw = fs::read_to_string(tmp.path()).unwrap();
        for line in raw.lines() {
            assert!(line.starts_with("SE:"), "encrypted schema line: {line:?}");
        }
        let wal = Wal::open(tmp.path(), Some(key)).unwrap();
        assert_eq!(wal.replay_schemas().unwrap(), stmts);
    }

    #[hegel::test]
    fn pbt_wal_rotate_creates_archive_with_pre_rotation_layers(tc: TestCase) {
        let initial: Vec<WalEntry<i64>> = (0..tc
            .draw(gs::integers::<usize>().min_value(1).max_value(4)))
            .map(|_| tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty())))
            .collect();
        let live_entries: Vec<WalEntry<i64>> = (0..tc
            .draw(gs::integers::<usize>().min_value(0).max_value(3)))
            .map(|_| tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty())))
            .collect();

        let dir = tempfile::tempdir().unwrap();
        let wal_path = dir.path().join("test.wal");
        let mut wal = Wal::open(&wal_path, None).unwrap();
        for e in &initial {
            wal.append(e).unwrap();
        }

        let schema_lines = wal.raw_schema_lines().unwrap();
        wal.rotate(&schema_lines, &live_entries).unwrap();

        // An archive file with a timestamp suffix must exist.
        let archives: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.starts_with("test.wal.") && s != "test.wal"
            })
            .collect();
        assert_eq!(archives.len(), 1, "exactly one archive must be created");

        // The archive must be replayable and contain the pre-rotation layers.
        let archive_path = archives[0].path();
        let mut archived_store: InMemory<i64> = InMemory::with_threshold(1024);
        Wal::open(&archive_path, None)
            .unwrap()
            .replay(&mut archived_store)
            .unwrap();

        let mut reference: InMemory<i64> = InMemory::with_threshold(1024);
        for e in &initial {
            if e.taus.is_empty() {
                continue;
            }
            let layer = Layer::new(
                e.layer_id,
                e.taus
                    .iter()
                    .map(|(s, end, v)| Tau::new(*s, *end, *v))
                    .collect(),
            );
            reference.append(&e.lens, layer).unwrap();
        }

        for e in &initial {
            if let Some((s, _, _)) = e.taus.first() {
                assert_eq!(
                    archived_store.at(&e.lens, *s),
                    reference.at(&e.lens, *s),
                    "archive must replay to pre-rotation state"
                );
            }
        }
    }

    #[hegel::test]
    fn pbt_wal_rotate_retains_only_supplied_entries(tc: TestCase) {
        let schemas = tc
            .draw(gs::vecs(gs::from_regex(r"CREATE LENS [a-z]+ int").fullmatch(true)).max_size(3));
        let initial: Vec<WalEntry<i64>> = (0..tc
            .draw(gs::integers::<usize>().min_value(1).max_value(5)))
            .map(|_| tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty())))
            .collect();
        let live_entries: Vec<WalEntry<i64>> = (0..tc
            .draw(gs::integers::<usize>().min_value(0).max_value(3)))
            .map(|_| tc.draw(entry_gen_i64().filter(|e| !e.taus.is_empty())))
            .collect();
        let tmp = NamedTempFile::new().unwrap();
        let mut wal = Wal::open(tmp.path(), None).unwrap();
        for s in &schemas {
            wal.append_schema(s).unwrap();
        }
        for e in &initial {
            wal.append(e).unwrap();
        }

        let schema_lines = wal.raw_schema_lines().unwrap();
        wal.rotate(&schema_lines, &live_entries).unwrap();

        // The rotate must leave exactly the supplied schema set, and the
        // post-rotation layer-id set must equal the live entries' ids.
        let post_stmts = wal.replay_schemas().unwrap();
        assert_eq!(post_stmts, schemas);
        let ids = wal.data_layer_ids().unwrap();
        let expected: std::collections::HashSet<u64> =
            live_entries.iter().map(|e| e.layer_id).collect();
        assert_eq!(ids, expected);
    }
}
