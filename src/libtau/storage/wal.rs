//! Write-ahead log for durable append semantics.
//!
//! # Design
//!
//! A dedicated background thread (`tau-wal`) owns the file handle.  Callers
//! enqueue serialised entries through a `SyncSender` and receive a
//! [`WalWaiter`] they can block on for durability confirmation.  The worker
//! batches everything that has accumulated in the channel and issues a single
//! `sync_data()` per batch (group-commit), then wakes every waiter at once.
//!
//! ```text
//! Database::append
//!     └─► Wal::push_entry  (serialise + enqueue, no IO)
//!         └─► WalWaiter::wait  (block until batch is fsynced)
//!             └─► Store::append (apply in-memory state)
//! ```
//!
//! On startup, `Wal::replay` feeds every persisted entry back into a fresh
//! `Store` so the in-memory view is reconstructed from durable state before
//! any new writes are accepted.
//!
//! # Serialisation
//!
//! Each WAL entry is one line: checksum header + serialised payload.
//!
//! ```text
//! <crc32> <layer_id> <lens_name> <start0>:<end0>:<value0> <start1>:<end1>:<value1> …
//! ```
//!
//! The CRC32 covers everything after the checksum field itself.

use crc32fast::Hasher;

use crate::libtau::model::{Layer, LayerId, Tau, Timestamp};
use crate::libtau::storage::Store;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
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

/// Build a CRC-prefixed `STMT` WAL line for a DDL statement payload.
pub fn serialise_stmt(text: &str) -> String {
    let payload = format!("STMT {text}");
    let checksum = crc32(&payload);
    format!("{checksum} {payload}")
}

/// Compute CRC32 of a string slice.
pub(crate) fn crc32(data: &str) -> u32 {
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

/// State shared between a caller and the WAL background thread for one
/// pending group-commit batch.
#[derive(Clone, PartialEq)]
enum WaitState {
    Pending,
    Done,
    Failed,
}

/// One queued WAL entry with its notification slot.
struct CommitEntry {
    line: String,
    notify: Arc<(Mutex<WaitState>, Condvar)>,
}

/// Handle for a caller blocked on a group-commit batch.
///
/// Returned by [`Wal::push_entry`].  Call [`WalWaiter::wait`] to block until
/// the batch including this entry has been fsynced.
pub struct WalWaiter(Arc<(Mutex<WaitState>, Condvar)>);

impl WalWaiter {
    /// Block until the batch this waiter belongs to has been fsynced.
    pub fn wait(self) -> io::Result<()> {
        let (lock, cvar) = &*self.0;
        let mut guard = lock.lock().unwrap();
        while *guard == WaitState::Pending {
            guard = cvar.wait(guard).unwrap();
        }
        match *guard {
            WaitState::Done => Ok(()),
            WaitState::Failed => Err(io::Error::other("WAL group-commit failed")),
            WaitState::Pending => unreachable!(),
        }
    }
}

/// Append-only write-ahead log backed by a single flat file.
///
/// The actual file handle lives on a background thread.  Callers enqueue
/// entries via [`Wal::push_entry`]; the worker batches and fsyncs them.
pub struct Wal {
    tx: SyncSender<CommitEntry>,
    /// Kept so [`Wal::replay`] can reopen the file for reading independently
    /// of the writer thread's append-only handle.
    path: std::path::PathBuf,
    /// Dropped without joining when the `Wal` is dropped; the worker exits
    /// cleanly once the channel is closed.
    _worker: thread::JoinHandle<()>,
}

impl Wal {
    /// Open (or create) the WAL at `path` and start the background writer.
    #[instrument(fields(path = %path.as_ref().display()))]
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        debug!("opening WAL file");
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let writer = BufWriter::new(file);

        let (tx, rx) = mpsc::sync_channel::<CommitEntry>(4096);

        let worker = thread::Builder::new()
            .name("tau-wal".into())
            .spawn(move || wal_worker(writer, rx))
            .map_err(io::Error::other)?;

        Ok(Self {
            tx,
            path,
            _worker: worker,
        })
    }

    /// Enqueue a pre-serialised WAL line.  Returns a [`WalWaiter`] the caller
    /// can block on to confirm durability.  Non-blocking apart from channel
    /// backpressure when the queue is full.
    pub fn push_entry(&self, line: String) -> WalWaiter {
        let notify = Arc::new((Mutex::new(WaitState::Pending), Condvar::new()));
        let entry = CommitEntry {
            line,
            notify: notify.clone(),
        };
        // Channel send only fails if the receiver has hung up, which can only
        // happen if the worker panicked.  Mark the waiter as Failed in that
        // case so wait() reports the error rather than blocking forever.
        if self.tx.send(entry).is_err() {
            let (lock, cvar) = &*notify;
            *lock.lock().unwrap() = WaitState::Failed;
            cvar.notify_all();
        }
        WalWaiter(notify)
    }

    /// Serialise and append a single entry, blocking until it is fsynced.
    ///
    /// Convenience wrapper around `push_entry().wait()` retained for the
    /// in-process `Database::append` path and existing tests.
    pub fn append<V: Codec>(&self, entry: &WalEntry<V>) -> io::Result<()> {
        debug!(
            lens = %entry.lens,
            layer_id = entry.layer_id,
            tau_count = entry.taus.len(),
            "writing WAL entry"
        );
        self.push_entry(entry.serialise()).wait()
    }

    /// Path to the underlying WAL file.  Useful for tools that want to read
    /// the log without taking a writer lock (e.g. the executor's replay
    /// helper which decodes both DATA and STMT lines).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Enqueue a DDL statement for durability.  The payload is prefixed with
    /// `STMT ` and CRC32-checksummed identically to data entries; replay
    /// distinguishes the two by sniffing the first token after the checksum.
    pub fn push_stmt(&self, text: &str) -> WalWaiter {
        self.push_entry(serialise_stmt(text))
    }

    /// Replay every persisted entry into `store` in write order.
    ///
    /// Corrupt entries (checksum mismatch) are skipped with a warning.
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

            if let Some(entry) = WalEntry::<V>::deserialise(line) {
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

/// Background worker loop.  Owns the file handle for the lifetime of the
/// `Wal`.  Blocks on the first entry of every batch, then drains anything
/// else that arrived in the meantime so a single `sync_data()` covers them
/// all (group-commit).
fn wal_worker(mut writer: BufWriter<File>, rx: mpsc::Receiver<CommitEntry>) {
    let mut batch: Vec<Arc<(Mutex<WaitState>, Condvar)>> = Vec::new();
    loop {
        // Block for at least one entry.
        let first = match rx.recv() {
            Ok(e) => e,
            Err(_) => {
                debug!("WAL worker shutting down (channel closed)");
                return;
            }
        };

        batch.clear();
        let mut io_result: io::Result<()> = Ok(());

        if let Err(e) = writeln!(writer, "{}", first.line) {
            io_result = Err(e);
        }
        batch.push(first.notify);

        // Drain everything else that's already queued without blocking.
        while let Ok(extra) = rx.try_recv() {
            if io_result.is_ok()
                && let Err(e) = writeln!(writer, "{}", extra.line)
            {
                io_result = Err(e);
            }
            batch.push(extra.notify);
        }

        // One flush + fsync for the whole batch.
        if io_result.is_ok()
            && let Err(e) = writer.flush()
        {
            io_result = Err(e);
        }
        if io_result.is_ok()
            && let Err(e) = writer.get_ref().sync_data()
        {
            io_result = Err(e);
        }

        let final_state = if io_result.is_ok() {
            debug!(batch = batch.len(), "WAL batch fsynced");
            WaitState::Done
        } else {
            warn!(error = ?io_result.as_ref().err(), "WAL batch failed");
            WaitState::Failed
        };

        for notify in batch.drain(..) {
            let (lock, cvar) = &*notify;
            *lock.lock().unwrap() = final_state.clone();
            cvar.notify_all();
        }
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
            let wal = Wal::open(tmp.path()).unwrap();
            let entry = WalEntry::<i64> {
                layer_id: 1,
                lens: "x".to_string(),
                taus: vec![(0, 10, 42)],
            };
            wal.append(&entry).unwrap();
        }

        let mut store: InMemory<i64> = InMemory::new();
        let wal = Wal::open(tmp.path()).unwrap();
        wal.replay(&mut store).unwrap();

        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 10), None);
    }

    #[test]
    fn wal_replays_multiple_entries_in_order() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let wal = Wal::open(tmp.path()).unwrap();
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
        Wal::open(tmp.path()).unwrap().replay(&mut store).unwrap();

        // newest layer (id=2) must shadow the earlier one
        assert_eq!(store.at("s", 3), Some(1));
        assert_eq!(store.at("s", 7), Some(2));
        assert_eq!(store.at("s", 17), Some(1));
    }

    #[test]
    fn wal_replays_multiple_lenses() {
        let tmp = NamedTempFile::new().unwrap();
        {
            let wal = Wal::open(tmp.path()).unwrap();
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
        Wal::open(tmp.path()).unwrap().replay(&mut store).unwrap();

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
        Wal::open(tmp.path()).unwrap().replay(&mut store).unwrap();

        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 15), Some(99));
    }

    #[test]
    fn wal_open_creates_file_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.wal");
        assert!(!path.exists());
        Wal::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn wal_replay_on_empty_file_is_a_noop() {
        let tmp = NamedTempFile::new().unwrap();
        let mut store: InMemory<i64> = InMemory::new();
        Wal::open(tmp.path()).unwrap().replay(&mut store).unwrap();
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
        Wal::open(tmp.path()).unwrap().replay(&mut store).unwrap();

        // First and third entries replayed, second skipped
        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.at("x", 25), Some(7));
    }
}
