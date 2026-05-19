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

use crate::libtau::crypto;
use crate::libtau::model::{Layer, LayerId, Tau, Timestamp};
use crate::libtau::storage::Store;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use tracing::{debug, instrument, warn};

/// Encode a single value `V` to/from a string token with no whitespace or
/// colons.  Implementations ship for the primitive numeric types below.
pub trait Codec: Sized {
    fn encode(&self) -> String;
    fn decode(s: &str) -> Option<Self>;
}

macro_rules! impl_codec_display_parse {
    ($($t:ty),+) => {
        $(impl Codec for $t {
            fn encode(&self) -> String { self.to_string() }
            fn decode(s: &str) -> Option<Self> { s.parse().ok() }
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

/// One serialised append record.
#[derive(Debug, PartialEq)]
pub struct WalEntry<V> {
    pub layer_id: LayerId,
    pub lens: String,
    pub taus: Vec<(Timestamp, Timestamp, V)>,
}

impl<V: Codec> WalEntry<V> {
    /// Serialise to the wire format with checksum prefix.
    pub fn serialise(&self) -> String {
        let taus_str = self
            .taus
            .iter()
            .map(|(s, e, v)| format!("{}:{}:{}", s, e, v.encode()))
            .collect::<Vec<_>>()
            .join(" ");
        let payload = if self.taus.is_empty() {
            format!("{} {}", self.layer_id, self.lens)
        } else {
            format!("{} {} {}", self.layer_id, self.lens, taus_str)
        };
        let checksum = crc32(&payload);
        format!("{} {}", checksum, payload)
    }

    /// Deserialise from one line of the log file, verifying checksum.
    pub fn deserialise(line: &str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }

        // Split into checksum and payload
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

        let mut tokens = payload.splitn(3, ' ');
        let layer_id: LayerId = tokens.next()?.parse().ok()?;
        let lens = tokens.next()?.to_string();
        let rest = tokens.next().unwrap_or("");

        let taus = if rest.is_empty() {
            vec![]
        } else {
            rest.split(' ')
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
            lens,
            taus,
        })
    }
}

// TODO: add a truncation / rotation path — after a compaction checkpoint, entries
// whose layers are fully covered by the compacted layer can be discarded from
// the front of the file. Without this the WAL grows without bound.

/// Append-only write-ahead log backed by a single flat file.
pub struct Wal {
    writer: BufWriter<File>,
    path: std::path::PathBuf,
    key: Option<[u8; 32]>,
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
        Ok(Self {
            writer: BufWriter::new(file),
            path,
            key,
        })
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
        if let Some(key) = &self.key {
            let plaintext = entry.serialise();
            let blob = crypto::encrypt(key, plaintext.as_bytes());
            writeln!(self.writer, "E:{}", B64.encode(&blob))?;
        } else {
            writeln!(self.writer, "{}", entry.serialise())?;
        }
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
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
                let key = match &self.key {
                    Some(k) => k,
                    None => {
                        warn!("encrypted WAL entry found but no key configured, skipping");
                        skipped += 1;
                        continue;
                    }
                };
                let blob = match B64.decode(rest) {
                    Ok(b) => b,
                    Err(_) => {
                        warn!("WAL entry base64 decode failed, skipping");
                        skipped += 1;
                        continue;
                    }
                };
                match crypto::decrypt(key, &blob) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(s) => s,
                        Err(_) => {
                            warn!("WAL entry decrypted but not valid UTF-8, skipping");
                            skipped += 1;
                            continue;
                        }
                    },
                    Err(_) => {
                        warn!("WAL entry decryption failed, skipping");
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

                store.append(&entry.lens, Layer::new(entry.layer_id, taus));
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libtau::storage::memory::InMemory;
    use tempfile::NamedTempFile;

    #[test]
    fn entry_serialises_with_checksum() {
        let entry: WalEntry<i64> = WalEntry {
            layer_id: 7,
            lens: "temp".to_string(),
            taus: vec![(0, 10, 42), (10, 20, 99)],
        };
        let line = entry.serialise();
        // Line should start with a hex number (checksum)
        assert!(line.chars().next().unwrap().is_numeric());
        // Deserialise should verify checksum
        let decoded = WalEntry::<i64>::deserialise(&line).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn entry_round_trips_with_taus() {
        let entry: WalEntry<i64> = WalEntry {
            layer_id: 7,
            lens: "temp".to_string(),
            taus: vec![(0, 10, 42), (10, 20, 99)],
        };
        let line = entry.serialise();
        let decoded = WalEntry::<i64>::deserialise(&line).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn entry_round_trips_with_no_taus() {
        let entry: WalEntry<i64> = WalEntry {
            layer_id: 1,
            lens: "empty".to_string(),
            taus: vec![],
        };
        let line = entry.serialise();
        let decoded = WalEntry::<i64>::deserialise(&line).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn entry_round_trips_float_values() {
        let entry: WalEntry<f64> = WalEntry {
            layer_id: 3,
            lens: "celsius".to_string(),
            taus: vec![(0, 10, 18.5), (10, 20, 20.0)],
        };
        let line = entry.serialise();
        let decoded = WalEntry::<f64>::deserialise(&line).unwrap();
        assert_eq!(decoded.layer_id, entry.layer_id);
        assert_eq!(decoded.lens, entry.lens);
        for ((s1, e1, v1), (s2, e2, v2)) in decoded.taus.iter().zip(entry.taus.iter()) {
            assert_eq!(s1, s2);
            assert_eq!(e1, e2);
            assert!((v1 - v2).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn entry_round_trips_bool_values() {
        let entry: WalEntry<bool> = WalEntry {
            layer_id: 2,
            lens: "alarm".to_string(),
            taus: vec![(0, 5, true), (5, 10, false)],
        };
        let line = entry.serialise();
        let decoded = WalEntry::<bool>::deserialise(&line).unwrap();
        assert_eq!(decoded, entry);
    }

    #[test]
    fn deserialise_returns_none_on_garbage() {
        assert!(WalEntry::<i64>::deserialise("not valid at all !!!").is_none());
        assert!(WalEntry::<i64>::deserialise("").is_none());
    }

    #[test]
    fn deserialise_returns_none_on_bad_tau_token() {
        // layer_id and lens are fine, tau token is malformed
        assert!(WalEntry::<i64>::deserialise("1 mylen 0:10:notanumber").is_none());
    }

    #[test]
    fn deserialise_returns_none_on_checksum_mismatch() {
        let entry: WalEntry<i64> = WalEntry {
            layer_id: 1,
            lens: "test".to_string(),
            taus: vec![(0, 10, 42)],
        };
        let line = entry.serialise();
        // Corrupt the checksum by changing first digit
        let corrupted = format!("9{}", &line[1..]);
        assert!(WalEntry::<i64>::deserialise(&corrupted).is_none());
    }

    #[test]
    fn codec_i64_encode_decode_round_trip() {
        let v: i64 = -999;
        assert_eq!(i64::decode(&v.encode()), Some(v));
    }

    #[test]
    fn codec_f64_encode_decode_round_trip() {
        let v: f64 = 2.5;
        let decoded = f64::decode(&v.encode()).unwrap();
        assert!((decoded - v).abs() < f64::EPSILON);
    }

    #[test]
    fn codec_bool_encode_decode() {
        assert_eq!(bool::decode("true"), Some(true));
        assert_eq!(bool::decode("false"), Some(false));
        assert_eq!(bool::decode("maybe"), None);
    }

    #[test]
    fn codec_decode_returns_none_on_empty_string() {
        assert_eq!(i64::decode(""), None);
    }

    #[test]
    fn wal_appends_and_replays_into_store() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), None).unwrap();
            let entry = WalEntry::<i64> {
                layer_id: 1,
                lens: "x".to_string(),
                taus: vec![(0, 10, 42)],
            };
            wal.append(&entry).unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();

        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 10), None);
    }

    #[test]
    fn wal_replays_multiple_entries_in_order() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), None).unwrap();
            wal.append(&WalEntry::<i64> {
                layer_id: 1,
                lens: "s".to_string(),
                taus: vec![(0, 20, 1)],
            })
            .unwrap();
            wal.append(&WalEntry::<i64> {
                layer_id: 2,
                lens: "s".to_string(),
                taus: vec![(5, 15, 2)],
            })
            .unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();

        // newest layer (id=2) must shadow the earlier one
        assert_eq!(store.at("s", 3), Some(1));
        assert_eq!(store.at("s", 7), Some(2));
        assert_eq!(store.at("s", 17), Some(1));
    }

    #[test]
    fn wal_replays_multiple_lenses() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), None).unwrap();
            wal.append(&WalEntry::<i64> {
                layer_id: 1,
                lens: "a".to_string(),
                taus: vec![(0, 10, 10)],
            })
            .unwrap();
            wal.append(&WalEntry::<i64> {
                layer_id: 2,
                lens: "b".to_string(),
                taus: vec![(0, 10, 20)],
            })
            .unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();

        assert_eq!(store.at("a", 5), Some(10));
        assert_eq!(store.at("b", 5), Some(20));
    }

    #[test]
    fn wal_skips_blank_lines_during_replay() {
        let tmp = NamedTempFile::new().unwrap();
        // write entries with checksums and a blank line
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(tmp.path())
                .unwrap();
            // Valid entry with checksum
            writeln!(f, "{} 1 x 0:10:42", crc32("1 x 0:10:42")).unwrap();
            writeln!(f).unwrap(); // blank
            writeln!(f, "{} 2 x 10:20:99", crc32("2 x 10:20:99")).unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();

        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 15), Some(99));
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

    #[test]
    fn wal_skips_corrupted_entry() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(tmp.path())
                .unwrap();
            // Valid entry
            writeln!(f, "{} 1 x 0:10:42", crc32("1 x 0:10:42")).unwrap();
            // Corrupted entry (wrong checksum)
            writeln!(f, "999999 2 x 5:15:99").unwrap();
            // Another valid entry
            writeln!(f, "{} 3 x 20:30:7", crc32("3 x 20:30:7")).unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();

        // First and third entries replayed, second skipped
        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 25), Some(7));
    }

    #[test]
    fn wal_encrypted_roundtrip() {
        let key = [0x42u8; 32];
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), Some(key)).unwrap();
            wal.append(&WalEntry::<i64> {
                layer_id: 1,
                lens: "secret".to_string(),
                taus: vec![(0, 10, 99)],
            })
            .unwrap();
        }

        // File must not contain the plaintext value
        let raw = std::fs::read_to_string(tmp.path()).unwrap();
        assert!(!raw.contains("99"), "plaintext value leaked into WAL file");
        assert!(raw.starts_with("E:"), "encrypted line must start with E:");

        // Replay with correct key succeeds
        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), Some(key))
            .unwrap()
            .replay(&mut store)
            .unwrap();
        assert_eq!(store.at("secret", 5), Some(99));
    }

    #[test]
    fn wal_encrypted_replay_without_key_skips_entries() {
        let key = [0x11u8; 32];
        let tmp = NamedTempFile::new().unwrap();
        {
            let mut wal = Wal::open(tmp.path(), Some(key)).unwrap();
            wal.append(&WalEntry::<i64> {
                layer_id: 1,
                lens: "x".to_string(),
                taus: vec![(0, 10, 7)],
            })
            .unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path(), None)
            .unwrap()
            .replay(&mut store)
            .unwrap();
        assert_eq!(
            store.at("x", 5),
            None,
            "entries must be skipped without key"
        );
    }
}
