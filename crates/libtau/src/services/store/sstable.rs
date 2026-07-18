//! SSTable-based on-disk backend: immutable, block-skippable **runs** plus a
//! small durable **manifest**, with newest-wins resolved at **read time**
//! (MVCC) instead of write time.
//!
//! # Why this exists
//!
//! [`Disk`](crate::services::store::Disk) rewrites the *entire* accumulated state into
//! one file on every checkpoint. That is fine at small scale but does not
//! survive billions of points. Tau's append/compact/newest-wins model is
//! already a naive LSM tree — this backend makes that explicit and on-disk:
//!
//! - a **memtable** (the same `Arc<[Layer]>` RCU stack `InMemory` uses) absorbs
//!   appends;
//! - `checkpoint_flush` moves the memtable into a new **immutable run file**
//!   instead of rewriting anything that already exists on disk;
//! - a **manifest** (small, atomically rewritten) lists the live run ids;
//! - reads merge the memtable with the runs, resolving newest-wins and
//!   `AS OF` **at read time**: stab the covering versions, take
//!   `argmax written_at` (optionally `<= as_of`).
//!
//! # Compaction trigger
//!
//! A lens's data can be split across the memtable and any number of runs, but
//! there is exactly **one** place that decides when to compact it:
//! [`Sstable::consolidate_lens`], fired from [`Store::append`] whenever
//! `layer_count[lens]` (a running total spanning memtable *and* every run,
//! persisted in the manifest) exceeds `compact_threshold` — the identical
//! condition and identical scope `InMemory::append` uses on its single
//! unified `Vec<Layer>`. This one-trigger design replaces an earlier version
//! with three overlapping, partial triggers (memtable-local count, an
//! unconditional per-checkpoint absorb loop, and a run-count threshold reusing
//! the same field for a different unit) whose gaps let a lens's data go
//! un-consolidated in specific interleavings — invisible to `AT`/`RANGE`/
//! `AS OF` (newest-wins doesn't care how many redundant older entries exist)
//! but not to `REDUCE … USING sum`/`avg`, which deliberately counts raw,
//! unmerged segments and so is directly sensitive to how many physical layers
//! survive. `gc_runs` (merging run *files*, not lens data) is a separate,
//! purely space-driven optimisation gated by its own `run_gc_threshold` — it
//! never needs to run for `consolidate_lens`'s guarantee to hold.
//!
//! # Run file format
//!
//! ```text
//! header: MAGIC "TAUR" (4) + VERSION (1) + FLAGS (1, bit0=encrypted) + CRC32 (4, over magic+version+flags)
//! body:   zstd-compressed, optionally AES-256-GCM-encrypted:
//!           entry_count u32 LE
//!           entries, sorted by (lens, coords[0].lo, coords[1..], written_at desc):
//!             lens (len u32 LE + utf8), layer_id u64 LE, written_at i64 LE, epoch u64 LE,
//!             arity u8, arity * (lo i64 LE, hi i64 LE), value (Codec-encoded)
//! footer: uncompressed (so skip-checks never decompress the body), optionally encrypted:
//!           lens_count u32 LE, then per lens: name, min_start i64 LE, max_end i64 LE,
//!           count u32 LE, bloom_n_bits u32 LE, bloom_byte_len u32 LE, bloom bytes
//! trailer: footer_len u32 LE (last 4 bytes of the file — lets a reader seek from EOF)
//! ```
//!
//! A run is skipped **without decoding its body** when the footer proves the
//! queried lens is absent, its `[min_start, max_end)` cannot cover the query,
//! or (for a point query) its bloom filter rules the point out.
//!
//! `epoch` lets `DROP LENS` followed by `CREATE LENS` of the same name shadow
//! all pre-drop entries without physically touching old run files: `drop_lens`
//! bumps the lens's epoch (persisted in the manifest); reads only consider
//! entries at the *current* epoch. This is the same idea as `written_at`
//! MVCC, one level coarser.
//!
//! # Scoping note
//!
//! A relevant run's body is decoded in full (all lenses it contains), not just
//! the queried lens — there is no intra-run block index yet, only run-level
//! skipping via the footer. That is the natural next optimisation; it was left
//! out here to keep this landing reviewable. A prior evaluation of intra-layer
//! indexing (interval tree vs. linear fast-skip) reached the same conclusion
//! for this backend's runs as it did for in-memory layers — see
//! `storage/README.md`.

use crate::crypto;
use crate::model::{Layer, LayerId, Tau, Timestamp};
use crate::services::store::codec::{Codec, DEFAULT_ZSTD_LEVEL};
use crate::services::store::layers::compact_layers;
use crate::services::store::store::{Store, layers_get, scan_layers};
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

const RUN_MAGIC: &[u8] = b"TAUR";
const RUN_VERSION: u8 = 1;
const MANIFEST_MAGIC: &[u8] = b"TAUM";
const MANIFEST_VERSION: u8 = 1;
const FLAG_ENCRYPTED: u8 = 0x01;
const HEADER_LEN: usize = 4 + 1 + 1 + 4; // magic + version + flags + crc32
const MAX_AXES: usize = 64;
/// Upper bound on a decoded string / bloom filter length, guarding against a
/// corrupted length prefix triggering an oversized allocation.
const MAX_PREALLOC: usize = 1 << 24;

// ---------------------------------------------------------------------------
// primitive byte IO — deliberately duplicated from `disk.rs` rather than
// shared, so this module cannot destabilise the already-hardened Disk format.
// ---------------------------------------------------------------------------

fn checksum(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

fn write_str<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    w.write_all(&(s.len() as u32).to_le_bytes())?;
    w.write_all(s.as_bytes())
}

fn read_str<R: Read>(r: &mut R) -> io::Result<String> {
    let len = read_u32(r)? as usize;
    if len > MAX_PREALLOC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "string too long",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_le_bytes(buf))
}

fn read_i64<R: Read>(r: &mut R) -> io::Result<i64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

fn write_header(magic: &[u8], version: u8, flags: u8) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(magic);
    header.push(version);
    header.push(flags);
    let crc = checksum(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    header
}

fn read_header(f: &mut File, magic: &[u8], version: u8) -> io::Result<u8> {
    let mut header = [0u8; HEADER_LEN];
    f.read_exact(&mut header)?;
    if &header[0..4] != magic {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad magic"));
    }
    let got_version = header[4];
    if got_version != version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported version: expected {version}, got {got_version}"),
        ));
    }
    let flags = header[5];
    let stored_crc = u32::from_le_bytes(header[6..10].try_into().unwrap());
    if checksum(&header[0..6]) != stored_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "header checksum mismatch",
        ));
    }
    Ok(flags)
}

fn atomic_write(path: &Path, bytes: &[&[u8]]) -> io::Result<()> {
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    {
        let mut f = BufWriter::new(File::create(&tmp)?);
        for chunk in bytes {
            f.write_all(chunk)?;
        }
        f.flush()?;
    }
    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// bloom filter — point-miss rejection per lens per run
// ---------------------------------------------------------------------------

struct Bloom {
    bits: Vec<u64>,
    n_bits: usize,
    k: u64,
}

impl Bloom {
    fn new(expected_items: usize) -> Self {
        let n_bits = (expected_items.max(1) * 10).max(64);
        let words = n_bits.div_ceil(64);
        Self {
            bits: vec![0u64; words],
            n_bits,
            k: 3,
        }
    }

    fn from_parts(n_bits: usize, bytes: &[u8]) -> Self {
        let words = n_bits.div_ceil(64);
        let mut bits = vec![0u64; words];
        for (i, chunk) in bytes.chunks(8).enumerate() {
            if i >= words {
                break;
            }
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            bits[i] = u64::from_le_bytes(buf);
        }
        Self { bits, n_bits, k: 3 }
    }

    fn to_bytes(&self) -> Vec<u8> {
        self.bits.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn hash_pair(key: Timestamp) -> (u64, u64) {
        let x = key as u64;
        let h1 = x.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(31);
        let h2 = (x ^ 0xD6E8_FEB8_6659_FD93)
            .wrapping_mul(0xBF58_476D_1CE4_E5B9)
            .rotate_left(27);
        (h1, h2)
    }

    fn insert(&mut self, key: Timestamp) {
        if self.n_bits == 0 {
            return;
        }
        let (h1, h2) = Self::hash_pair(key);
        for i in 0..self.k {
            let idx = (h1.wrapping_add(i.wrapping_mul(h2))) as usize % self.n_bits;
            self.bits[idx / 64] |= 1 << (idx % 64);
        }
    }

    /// `false` means definitely absent; `true` means maybe present (or the
    /// filter is degenerate, in which case this fails open rather than
    /// wrongly skipping a run that might hold the point).
    fn might_contain(&self, key: Timestamp) -> bool {
        if self.n_bits == 0 {
            return true;
        }
        let (h1, h2) = Self::hash_pair(key);
        (0..self.k).all(|i| {
            let idx = (h1.wrapping_add(i.wrapping_mul(h2))) as usize % self.n_bits;
            self.bits[idx / 64] & (1 << (idx % 64)) != 0
        })
    }
}

// ---------------------------------------------------------------------------
// run entries + footer stats
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RunEntry<V> {
    lens: String,
    layer_id: LayerId,
    written_at: i64,
    epoch: u64,
    coords: Vec<(Timestamp, Timestamp)>,
    value: V,
}

fn write_run_entry<V: Codec, W: Write>(w: &mut W, e: &RunEntry<V>) -> io::Result<()> {
    write_str(w, &e.lens)?;
    w.write_all(&e.layer_id.to_le_bytes())?;
    w.write_all(&e.written_at.to_le_bytes())?;
    w.write_all(&e.epoch.to_le_bytes())?;
    w.write_all(&[e.coords.len() as u8])?;
    for &(lo, hi) in &e.coords {
        w.write_all(&lo.to_le_bytes())?;
        w.write_all(&hi.to_le_bytes())?;
    }
    e.value.write_encoded(w)
}

fn read_run_entry<V: Codec, R: Read>(r: &mut R) -> io::Result<RunEntry<V>> {
    let lens = read_str(r)?;
    let layer_id = read_u64(r)?;
    let written_at = read_i64(r)?;
    let epoch = read_u64(r)?;
    let mut arity_buf = [0u8; 1];
    r.read_exact(&mut arity_buf)?;
    let arity = arity_buf[0] as usize;
    if arity == 0 || arity > MAX_AXES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid run entry arity: {arity}"),
        ));
    }
    let mut coords = Vec::with_capacity(arity);
    for _ in 0..arity {
        let lo = read_i64(r)?;
        let hi = read_i64(r)?;
        coords.push((lo, hi));
    }
    let value = V::read_encoded(r)?;
    Ok(RunEntry {
        lens,
        layer_id,
        written_at,
        epoch,
        coords,
        value,
    })
}

/// Per-lens skip-check stats read from a run's footer without decoding its body.
struct LensRunStats {
    min_start: Timestamp,
    max_end: Timestamp,
    count: usize,
    bloom: Bloom,
}

struct LensBuilder {
    min_start: Timestamp,
    max_end: Timestamp,
    count: usize,
    intervals: Vec<(Timestamp, Timestamp)>,
}

/// Bucket width for the range bloom filter: the run's valid-time extent for a
/// lens divided into (up to) 256 buckets, adapted to whatever timestamp unit
/// the caller uses (seconds, ms, ns, …) instead of assuming one.
fn bloom_bucket_width(min_start: Timestamp, max_end: Timestamp) -> i64 {
    ((max_end - min_start) / 256).max(1)
}

fn bloom_bucket_of(t: Timestamp, min_start: Timestamp, width: i64) -> i64 {
    (t - min_start).div_euclid(width)
}

/// Insert every bucket a `[lo, hi)` interval passes through, so a point query
/// for any `t` inside the interval maps to a bucket that was inserted. This is
/// what makes the filter sound for *interval* membership rather than exact
/// key membership: a plain per-`lo` bloom filter would (and did) reject valid
/// query points that fall strictly inside an interval.
fn bloom_insert_interval(
    bloom: &mut Bloom,
    lo: Timestamp,
    hi: Timestamp,
    min_start: Timestamp,
    width: i64,
) {
    let first = bloom_bucket_of(lo, min_start, width);
    let last = bloom_bucket_of(hi.saturating_sub(1), min_start, width);
    let mut b = first;
    while b <= last {
        bloom.insert(b);
        b += 1;
    }
}

fn write_run<V: Codec>(
    path: &Path,
    entries: &[RunEntry<V>],
    key: Option<[u8; 32]>,
    level: i32,
) -> io::Result<HashMap<String, LensRunStats>> {
    let mut body = Vec::new();
    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let mut builders: HashMap<String, LensBuilder> = HashMap::default();
    for e in entries {
        write_run_entry(&mut body, e)?;
        let b = builders
            .entry(e.lens.clone())
            .or_insert_with(|| LensBuilder {
                min_start: Timestamp::MAX,
                max_end: Timestamp::MIN,
                count: 0,
                intervals: Vec::new(),
            });
        let (s, en) = e.coords[0];
        b.min_start = b.min_start.min(s);
        b.max_end = b.max_end.max(en);
        b.count += 1;
        b.intervals.push((s, en));
    }

    let compressed = zstd::encode_all(body.as_slice(), level).map_err(io::Error::other)?;
    let flags = if key.is_some() { FLAG_ENCRYPTED } else { 0 };
    let body_bytes = match key {
        Some(k) => crypto::encrypt(&k, &compressed)?,
        None => compressed,
    };

    let mut stats: HashMap<String, LensRunStats> = HashMap::default();
    let mut footer = Vec::new();
    footer.extend_from_slice(&(builders.len() as u32).to_le_bytes());
    for (name, b) in &builders {
        let mut bloom = Bloom::new(b.count);
        let width = bloom_bucket_width(b.min_start, b.max_end);
        for &(lo, hi) in &b.intervals {
            bloom_insert_interval(&mut bloom, lo, hi, b.min_start, width);
        }
        let n_bits = bloom.n_bits;
        let bloom_bytes = bloom.to_bytes();
        write_str(&mut footer, name)?;
        footer.extend_from_slice(&b.min_start.to_le_bytes());
        footer.extend_from_slice(&b.max_end.to_le_bytes());
        footer.extend_from_slice(&(b.count as u32).to_le_bytes());
        footer.extend_from_slice(&(n_bits as u32).to_le_bytes());
        footer.extend_from_slice(&(bloom_bytes.len() as u32).to_le_bytes());
        footer.extend_from_slice(&bloom_bytes);
        stats.insert(
            name.clone(),
            LensRunStats {
                min_start: b.min_start,
                max_end: b.max_end,
                count: b.count,
                bloom,
            },
        );
    }
    let footer_bytes = match key {
        Some(k) => crypto::encrypt(&k, &footer)?,
        None => footer,
    };

    let header = write_header(RUN_MAGIC, RUN_VERSION, flags);
    atomic_write(
        path,
        &[
            &header,
            &body_bytes,
            &footer_bytes,
            &(footer_bytes.len() as u32).to_le_bytes(),
        ],
    )?;
    Ok(stats)
}

fn read_run_footer(
    path: &Path,
    key: Option<[u8; 32]>,
) -> io::Result<HashMap<String, LensRunStats>> {
    let mut f = File::open(path)?;
    let flags = read_header(&mut f, RUN_MAGIC, RUN_VERSION)?;
    let file_len = f.metadata()?.len();
    if file_len < HEADER_LEN as u64 + 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "run file too short",
        ));
    }
    f.seek(SeekFrom::End(-4))?;
    let footer_len = read_u32(&mut f)? as u64;
    if footer_len > file_len - HEADER_LEN as u64 - 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid run footer length",
        ));
    }
    f.seek(SeekFrom::End(-4 - footer_len as i64))?;
    let mut footer_bytes = vec![0u8; footer_len as usize];
    f.read_exact(&mut footer_bytes)?;
    let footer_plain = if flags & FLAG_ENCRYPTED != 0 {
        let k = key.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "run is encrypted but no key configured",
            )
        })?;
        crypto::decrypt(&k, &footer_bytes)?
    } else {
        footer_bytes
    };
    parse_footer(&footer_plain)
}

fn parse_footer(bytes: &[u8]) -> io::Result<HashMap<String, LensRunStats>> {
    let mut cur = Cursor::new(bytes);
    let n = read_u32(&mut cur)? as usize;
    let mut out = HashMap::default();
    for _ in 0..n {
        let name = read_str(&mut cur)?;
        let min_start = read_i64(&mut cur)?;
        let max_end = read_i64(&mut cur)?;
        let count = read_u32(&mut cur)? as usize;
        let n_bits = read_u32(&mut cur)? as usize;
        let blen = read_u32(&mut cur)? as usize;
        if blen > MAX_PREALLOC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bloom filter too large",
            ));
        }
        let mut bloom_bytes = vec![0u8; blen];
        cur.read_exact(&mut bloom_bytes)?;
        out.insert(
            name,
            LensRunStats {
                min_start,
                max_end,
                count,
                bloom: Bloom::from_parts(n_bits, &bloom_bytes),
            },
        );
    }
    Ok(out)
}

fn read_run_entries<V: Codec>(path: &Path, key: Option<[u8; 32]>) -> io::Result<Vec<RunEntry<V>>> {
    let mut f = File::open(path)?;
    let flags = read_header(&mut f, RUN_MAGIC, RUN_VERSION)?;
    let file_len = f.metadata()?.len();
    f.seek(SeekFrom::End(-4))?;
    let footer_len = read_u32(&mut f)? as u64;
    let body_start = HEADER_LEN as u64;
    if footer_len + 4 + body_start > file_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid run layout",
        ));
    }
    let body_end = file_len - 4 - footer_len;
    if body_end < body_start {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid run layout",
        ));
    }
    f.seek(SeekFrom::Start(body_start))?;
    let mut body_bytes = vec![0u8; (body_end - body_start) as usize];
    f.read_exact(&mut body_bytes)?;
    let compressed = if flags & FLAG_ENCRYPTED != 0 {
        let k = key.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "run is encrypted but no key configured",
            )
        })?;
        crypto::decrypt(&k, &body_bytes)?
    } else {
        body_bytes
    };
    let decompressed = zstd::decode_all(compressed.as_slice())?;
    let mut cur = Cursor::new(decompressed);
    let count = read_u32(&mut cur)? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        out.push(read_run_entry::<V, _>(&mut cur)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// manifest — schema DDL, live lens set + epochs, live run ids
// ---------------------------------------------------------------------------

struct ManifestData {
    schema: Vec<String>,
    live_lenses: Vec<String>,
    epochs: Vec<(String, u64)>,
    run_ids: Vec<u64>,
    next_run_id: u64,
    /// Per-lens total layer count spanning memtable + every run — the
    /// `consolidate_lens` trigger's state. Must be persisted: it is not
    /// derivable cheaply from the manifest's other fields (run footers only
    /// carry tau counts, not layer counts), and starting it at 0 after a
    /// restart would undercount a lens's pre-restart backlog, silently
    /// raising its effective compaction threshold.
    layer_counts: Vec<(String, usize)>,
}

fn write_manifest(path: &Path, data: &ManifestData, key: Option<[u8; 32]>) -> io::Result<()> {
    let mut body = Vec::new();
    body.extend_from_slice(&(data.schema.len() as u32).to_le_bytes());
    for s in &data.schema {
        write_str(&mut body, s)?;
    }
    body.extend_from_slice(&(data.live_lenses.len() as u32).to_le_bytes());
    for s in &data.live_lenses {
        write_str(&mut body, s)?;
    }
    body.extend_from_slice(&(data.epochs.len() as u32).to_le_bytes());
    for (name, ep) in &data.epochs {
        write_str(&mut body, name)?;
        body.extend_from_slice(&ep.to_le_bytes());
    }
    body.extend_from_slice(&(data.run_ids.len() as u32).to_le_bytes());
    for id in &data.run_ids {
        body.extend_from_slice(&id.to_le_bytes());
    }
    body.extend_from_slice(&data.next_run_id.to_le_bytes());
    body.extend_from_slice(&(data.layer_counts.len() as u32).to_le_bytes());
    for (name, count) in &data.layer_counts {
        write_str(&mut body, name)?;
        body.extend_from_slice(&(*count as u64).to_le_bytes());
    }
    body.extend_from_slice(&checksum(&body).to_le_bytes());

    let flags = if key.is_some() { FLAG_ENCRYPTED } else { 0 };
    let payload = match key {
        Some(k) => crypto::encrypt(&k, &body)?,
        None => body,
    };
    let header = write_header(MANIFEST_MAGIC, MANIFEST_VERSION, flags);
    atomic_write(path, &[&header, &payload])
}

fn read_manifest(path: &Path, key: Option<[u8; 32]>) -> io::Result<ManifestData> {
    let mut f = File::open(path)?;
    let flags = read_header(&mut f, MANIFEST_MAGIC, MANIFEST_VERSION)?;
    let mut payload = Vec::new();
    f.read_to_end(&mut payload)?;
    let body = if flags & FLAG_ENCRYPTED != 0 {
        let k = key.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest is encrypted but no key configured",
            )
        })?;
        crypto::decrypt(&k, &payload)?
    } else {
        payload
    };
    if body.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest body too short",
        ));
    }
    let (content, crc_bytes) = body.split_at(body.len() - 4);
    let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());
    if checksum(content) != stored_crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest body checksum mismatch",
        ));
    }
    let mut cur = Cursor::new(content);
    let schema = read_string_vec(&mut cur)?;
    let live_lenses = read_string_vec(&mut cur)?;
    let epoch_count = read_u32(&mut cur)? as usize;
    let mut epochs = Vec::with_capacity(epoch_count.min(1 << 16));
    for _ in 0..epoch_count {
        let name = read_str(&mut cur)?;
        let ep = read_u64(&mut cur)?;
        epochs.push((name, ep));
    }
    let run_count = read_u32(&mut cur)? as usize;
    let mut run_ids = Vec::with_capacity(run_count.min(1 << 16));
    for _ in 0..run_count {
        run_ids.push(read_u64(&mut cur)?);
    }
    let next_run_id = read_u64(&mut cur)?;
    let layer_count_n = read_u32(&mut cur)? as usize;
    let mut layer_counts = Vec::with_capacity(layer_count_n.min(1 << 16));
    for _ in 0..layer_count_n {
        let name = read_str(&mut cur)?;
        let count = read_u64(&mut cur)? as usize;
        layer_counts.push((name, count));
    }
    Ok(ManifestData {
        schema,
        live_lenses,
        epochs,
        run_ids,
        next_run_id,
        layer_counts,
    })
}

fn read_string_vec<R: Read>(r: &mut R) -> io::Result<Vec<String>> {
    let n = read_u32(r)? as usize;
    let mut out = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        out.push(read_str(r)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sstable
// ---------------------------------------------------------------------------

struct RunHandle<V> {
    id: u64,
    path: PathBuf,
    stats: HashMap<String, LensRunStats>,
    /// Lazily-populated, whole-run decode cache: the expensive part of a read
    /// is decompressing the run body, and a run is immutable once written, so
    /// the first reader pays that cost and every later query against the same
    /// run (regardless of lens) reuses it. Without this, every `AT`/`RANGE`/
    /// `REDUCE`/`HISTORY` query re-decodes the whole run from scratch, and
    /// since a run's size grows with total appended data, per-query cost grows
    /// with total ops appended so far — quadratic total cost over a run.
    body: RwLock<Option<Arc<Vec<RunEntry<V>>>>>,
}

struct SstableInner<V> {
    memtable: HashMap<String, Arc<[Layer<V>]>>,
    /// Oldest first; a run's position also serves as its recency tiebreak.
    runs: Vec<RunHandle<V>>,
    schema: Vec<String>,
    live_lenses: HashSet<String>,
    lens_epoch: HashMap<String, u64>,
    next_run_id: u64,
    /// Run ids already folded into the memtable per lens by
    /// [`Sstable::consolidate_lens`] (or known to hold nothing at that lens's
    /// current epoch) — an in-memory-only cache so a run already accounted
    /// for is never decoded twice. Not persisted: on reopen it starts empty
    /// and the first consolidation for each lens simply rebuilds it.
    absorbed: HashMap<String, HashSet<u64>>,
    /// Total layer count per lens spanning memtable + every run — the single
    /// signal `Store::append` checks against `compact_threshold`. See the
    /// module doc's "Compaction trigger" section. Persisted (manifest).
    layer_count: HashMap<String, usize>,
}

/// On-disk SSTable + MVCC store. See the module docs for the format and design.
pub struct Sstable<V> {
    base: PathBuf,
    key: Option<[u8; 32]>,
    compression_level: i32,
    /// Per-lens layer-count threshold that triggers `consolidate_lens` (same
    /// knob and semantics as `InMemory`/`Disk`'s `compact_threshold`).
    compact_threshold: usize,
    /// Live run *file* count threshold that triggers `gc_runs` — a purely
    /// space-driven optimisation, independent of `compact_threshold` and not
    /// required for `consolidate_lens`'s correctness guarantee to hold.
    run_gc_threshold: usize,
    /// Per-lens compaction threshold overrides.  `Some(n)` means this lens
    /// compacts when it exceeds `n` layers (0 = never); absent means use the
    /// server-wide `compact_threshold`.
    lens_compact: HashMap<String, usize>,
    inner: RwLock<SstableInner<V>>,
}

impl<V: Clone + PartialEq + Codec> Sstable<V> {
    fn manifest_path(base: &Path) -> PathBuf {
        PathBuf::from(format!("{}.manifest", base.display()))
    }

    fn run_path(base: &Path, id: u64) -> PathBuf {
        PathBuf::from(format!("{}.run.{id}", base.display()))
    }

    /// Open the store rooted at `base` (no extension — `.manifest` and
    /// `.run.<id>` files are derived from it), creating a fresh empty store if
    /// no manifest exists yet.
    pub fn open(base: impl AsRef<Path>, key: Option<[u8; 32]>) -> io::Result<Self> {
        let base = base.as_ref().to_path_buf();
        if let Some(parent) = base.parent() {
            fs::create_dir_all(parent)?;
        }
        let manifest_path = Self::manifest_path(&base);
        let inner = if manifest_path.exists() {
            let data = read_manifest(&manifest_path, key)?;
            let mut runs = Vec::with_capacity(data.run_ids.len());
            for id in &data.run_ids {
                let path = Self::run_path(&base, *id);
                let stats = read_run_footer(&path, key)?;
                runs.push(RunHandle {
                    id: *id,
                    path,
                    stats,
                    body: RwLock::new(None),
                });
            }
            SstableInner {
                memtable: HashMap::default(),
                runs,
                schema: data.schema,
                live_lenses: data.live_lenses.into_iter().collect(),
                lens_epoch: data.epochs.into_iter().collect(),
                next_run_id: data.next_run_id.max(1),
                absorbed: HashMap::default(),
                layer_count: data.layer_counts.into_iter().collect(),
            }
        } else {
            SstableInner {
                memtable: HashMap::default(),
                runs: Vec::new(),
                schema: Vec::new(),
                live_lenses: HashSet::default(),
                lens_epoch: HashMap::default(),
                next_run_id: 1,
                absorbed: HashMap::default(),
                layer_count: HashMap::default(),
            }
        };
        let store = Self {
            base,
            key,
            compression_level: DEFAULT_ZSTD_LEVEL,
            compact_threshold: crate::services::store::store::COMPACT_THRESHOLD,
            run_gc_threshold: crate::services::store::store::COMPACT_THRESHOLD,
            lens_compact: HashMap::default(),
            inner: RwLock::new(inner),
        };
        if !manifest_path.exists() {
            let g = store.inner.read().expect("sstable lock poisoned");
            store.persist_manifest_locked(&g)?;
        }
        Ok(store)
    }

    pub fn set_compact_threshold(&mut self, n: usize) {
        self.compact_threshold = n;
    }

    pub fn set_run_gc_threshold(&mut self, n: usize) {
        self.run_gc_threshold = n;
    }

    pub fn set_compression_level(&mut self, level: i32) {
        self.compression_level = level;
    }

    fn persist_manifest_locked(&self, inner: &SstableInner<V>) -> io::Result<()> {
        let data = ManifestData {
            schema: inner.schema.clone(),
            live_lenses: inner.live_lenses.iter().cloned().collect(),
            epochs: inner
                .lens_epoch
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            run_ids: inner.runs.iter().map(|r| r.id).collect(),
            next_run_id: inner.next_run_id,
            layer_counts: inner
                .layer_count
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        };
        write_manifest(&Self::manifest_path(&self.base), &data, self.key)
    }

    /// Reconstruct this run's taus for `lens` at `epoch` into `Layer<V>`s,
    /// grouped by `layer_id` (ascending — age order within the run) so the
    /// result composes with [`layers_get`]/[`scan_layers`]. The run body
    /// (all lenses it contains — see the module scoping note) is decoded once
    /// and cached on `run.body`; every later call against the same run, for
    /// any lens, reuses it instead of re-decompressing from disk.
    fn run_layers_for(
        run: &RunHandle<V>,
        key: Option<[u8; 32]>,
        lens: &str,
        epoch: u64,
    ) -> io::Result<Vec<Layer<V>>> {
        let body = {
            if let Some(cached) = run.body.read().ok().and_then(|g| g.clone()) {
                cached
            } else {
                let decoded = Arc::new(read_run_entries::<V>(&run.path, key)?);
                if let Ok(mut slot) = run.body.write() {
                    *slot = Some(decoded.clone());
                }
                decoded
            }
        };
        let mut groups: BTreeMap<LayerId, (i64, Vec<Tau<V>>)> = BTreeMap::new();
        for e in body.iter().filter(|e| e.lens == lens && e.epoch == epoch) {
            if let Some(tau) = Tau::try_new_nd(&e.coords, e.value.clone()) {
                groups
                    .entry(e.layer_id)
                    .or_insert_with(|| (e.written_at, Vec::new()))
                    .1
                    .push(tau);
            }
        }
        Ok(groups
            .into_iter()
            .filter_map(|(id, (written_at, taus))| Layer::try_new_nd_at(id, taus, written_at))
            .collect())
    }

    fn entry_sort_key(e: &RunEntry<V>) -> (String, Timestamp, Vec<(Timestamp, Timestamp)>, i64) {
        (
            e.lens.clone(),
            e.coords[0].0,
            e.coords[1..].to_vec(),
            -e.written_at, // descending
        )
    }

    /// The sole compaction trigger (see the module doc's "Compaction trigger"
    /// section): merge *every* physical fragment of `lens` — every
    /// not-yet-absorbed run plus the current memtable — into the canonical
    /// per-generation result, replace the memtable entry with it, and record
    /// the new total in `layer_count[lens]`. Runs actually pulled in have
    /// their ids recorded in `absorbed[lens]` and the lens's epoch bumped, so
    /// their now-redundant physical entries are ignored on read (reusing the
    /// same epoch mechanism `drop_lens` uses to shadow pre-drop data); a pure
    /// memtable-only consolidation (no runs yet, or all already absorbed)
    /// does neither, since there is nothing to invalidate.
    ///
    /// Called from exactly one place — [`Store::append`], when
    /// `layer_count[lens]` crosses `compact_threshold` — so a lens's
    /// cross-fragment correctness is enforced under the identical condition
    /// and identical scope `InMemory::append` gets automatically from its one
    /// unified `Vec<Layer>`. Caller must hold `inner` for writing.
    fn consolidate_lens(&self, inner: &mut SstableInner<V>, lens: &str) -> io::Result<()> {
        let epoch = *inner.lens_epoch.get(lens).unwrap_or(&0);
        let mut layers: Vec<Layer<V>> = Vec::new();
        let mut absorbed_ids: Vec<u64> = Vec::new();

        if !inner.runs.is_empty() {
            // Prune ids for runs that no longer exist (deleted by a prior
            // `gc_runs`) so this set stays bounded by the current run count
            // instead of growing with the total number of runs ever created.
            let live_ids: HashSet<u64> = inner.runs.iter().map(|r| r.id).collect();
            if let Some(set) = inner.absorbed.get_mut(lens) {
                set.retain(|id| live_ids.contains(id));
            }
            let already = inner.absorbed.get(lens);
            let pending: Vec<u64> = inner
                .runs
                .iter()
                .filter(|run| {
                    run.stats.contains_key(lens) && !already.is_some_and(|s| s.contains(&run.id))
                })
                .map(|run| run.id)
                .collect();
            for run in inner.runs.iter().filter(|r| pending.contains(&r.id)) {
                layers.extend(Self::run_layers_for(run, self.key, lens, epoch)?);
            }
            absorbed_ids = pending;
        }
        if let Some(mem) = inner.memtable.get(lens) {
            layers.extend(mem.iter().cloned());
        }
        // compact_layers requires equal-written_at layers to be contiguous;
        // concatenating multiple runs' reconstructions does not guarantee
        // that on its own.
        layers.sort_by_key(|l| (l.written_at, l.id));
        compact_layers(&mut layers);

        if !absorbed_ids.is_empty() {
            inner
                .absorbed
                .entry(lens.to_string())
                .or_default()
                .extend(absorbed_ids);
            *inner.lens_epoch.entry(lens.to_string()).or_insert(0) += 1;
        }
        inner.layer_count.insert(lens.to_string(), layers.len());
        inner.memtable.insert(lens.to_string(), Arc::from(layers));
        self.persist_manifest_locked(inner)
    }

    /// Flush the current memtable into a new run file (no-op when empty),
    /// then GC old run files once their count crosses `run_gc_threshold`.
    /// Purely a durability/space operation: correctness never depends on
    /// this running, since [`Self::consolidate_lens`] already guarantees a
    /// lens's cross-fragment state whenever it matters. Caller must hold
    /// `inner` for writing.
    fn flush_memtable_to_run(&self, inner: &mut SstableInner<V>) -> io::Result<()> {
        if !inner.memtable.is_empty() {
            let mut entries: Vec<RunEntry<V>> = Vec::new();
            for (lens, layers) in inner.memtable.iter() {
                let epoch = *inner.lens_epoch.get(lens).unwrap_or(&0);
                for layer in layers.iter() {
                    for tau in layer.taus.iter() {
                        entries.push(RunEntry {
                            lens: lens.clone(),
                            layer_id: layer.id,
                            written_at: layer.written_at,
                            epoch,
                            coords: tau.coords.iter().map(|b| (b.lo, b.hi)).collect(),
                            value: tau.value.clone(),
                        });
                    }
                }
            }
            if !entries.is_empty() {
                entries.sort_by_key(Self::entry_sort_key);
                let id = inner.next_run_id;
                inner.next_run_id += 1;
                let path = Self::run_path(&self.base, id);
                let stats = write_run(&path, &entries, self.key, self.compression_level)?;
                inner.runs.push(RunHandle {
                    id,
                    path,
                    stats,
                    body: RwLock::new(None),
                });
            }
            inner.memtable.clear();
        }
        if inner.runs.len() > self.run_gc_threshold {
            self.gc_runs(inner)?;
        }
        Ok(())
    }

    /// Merge every live run into one, replaying the existing per-generation
    /// lossless `compact_layers` per lens so `AS OF`/`HISTORY` stay exact.
    /// Old run files are deleted only after the merged run is durably written
    /// and the manifest no longer references them, so a crash mid-GC never
    /// loses data (worst case: an orphaned run file next to a manifest that
    /// already ignores it). Purely a space optimisation — see
    /// [`Self::consolidate_lens`] for the actual correctness mechanism.
    fn gc_runs(&self, inner: &mut SstableInner<V>) -> io::Result<()> {
        if inner.runs.len() <= 1 {
            return Ok(());
        }
        let mut lens_names: HashSet<String> = HashSet::default();
        for run in &inner.runs {
            lens_names.extend(run.stats.keys().cloned());
        }
        let mut merged: Vec<RunEntry<V>> = Vec::new();
        for lens in &lens_names {
            let epoch = *inner.lens_epoch.get(lens).unwrap_or(&0);
            let mut layers: Vec<Layer<V>> = Vec::new();
            for run in &inner.runs {
                if !run.stats.contains_key(lens) {
                    continue;
                }
                layers.extend(Self::run_layers_for(run, self.key, lens, epoch)?);
            }
            if layers.is_empty() {
                continue;
            }
            // compact_layers requires equal-written_at layers to be
            // contiguous; concatenating multiple runs' reconstructions does
            // not guarantee that on its own.
            layers.sort_by_key(|l| (l.written_at, l.id));
            compact_layers(&mut layers);
            for layer in &layers {
                for tau in layer.taus.iter() {
                    merged.push(RunEntry {
                        lens: lens.clone(),
                        layer_id: layer.id,
                        written_at: layer.written_at,
                        epoch,
                        coords: tau.coords.iter().map(|b| (b.lo, b.hi)).collect(),
                        value: tau.value.clone(),
                    });
                }
            }
        }

        let old_paths: Vec<PathBuf> = inner.runs.iter().map(|r| r.path.clone()).collect();
        if merged.is_empty() {
            inner.runs.clear();
        } else {
            merged.sort_by_key(Self::entry_sort_key);
            let id = inner.next_run_id;
            inner.next_run_id += 1;
            let path = Self::run_path(&self.base, id);
            let stats = write_run(&path, &merged, self.key, self.compression_level)?;
            inner.runs = vec![RunHandle {
                id,
                path,
                stats,
                body: RwLock::new(None),
            }];
        }
        self.persist_manifest_locked(inner)?;
        for p in old_paths {
            let _ = fs::remove_file(p);
        }
        Ok(())
    }
}

impl<V: Clone + PartialEq + Codec + Send + Sync + 'static> Store<V> for Sstable<V> {
    fn append(&mut self, lens: &str, layer: Layer<V>) -> io::Result<bool> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| io::Error::other("sstable lock poisoned"))?;
        let mut mem: Vec<Layer<V>> = inner
            .memtable
            .get(lens)
            .map(|a| a.to_vec())
            .unwrap_or_default();
        mem.push(layer);
        inner.memtable.insert(lens.to_string(), Arc::from(mem));
        let before = inner.layer_count.get(lens).copied().unwrap_or(0) + 1;
        inner.layer_count.insert(lens.to_string(), before);
        let effective = self
            .lens_compact
            .get(lens)
            .copied()
            .unwrap_or(self.compact_threshold);
        let mut did_compact = false;
        if effective > 0 && before > effective {
            self.consolidate_lens(&mut inner, lens)?;
            let after = inner.layer_count.get(lens).copied().unwrap_or(0);
            did_compact = after < before;
        }
        inner.live_lenses.insert(lens.to_string());
        Ok(did_compact)
    }

    fn at(&self, lens: &str, t: Timestamp) -> Option<V> {
        self.get(lens, &[t], None)
    }

    fn get(&self, lens: &str, coords: &[Timestamp], as_of: Option<i64>) -> Option<V> {
        let &t0 = coords.first()?;
        let inner = self.inner.read().ok()?;
        let epoch = *inner.lens_epoch.get(lens).unwrap_or(&0);
        if let Some(mem) = inner.memtable.get(lens)
            && let Some(v) = layers_get(mem, coords, as_of)
        {
            return Some(v);
        }
        for run in inner.runs.iter().rev() {
            let Some(stats) = run.stats.get(lens) else {
                continue;
            };
            if stats.count == 0 || t0 < stats.min_start || t0 >= stats.max_end {
                continue;
            }
            let width = bloom_bucket_width(stats.min_start, stats.max_end);
            if !stats
                .bloom
                .might_contain(bloom_bucket_of(t0, stats.min_start, width))
            {
                continue;
            }
            let Ok(layers) = Self::run_layers_for(run, self.key, lens, epoch) else {
                continue;
            };
            if let Some(v) = layers_get(&layers, coords, as_of) {
                return Some(v);
            }
        }
        None
    }

    fn scan(
        &self,
        lens: &str,
        start: Timestamp,
        end: Timestamp,
        fixed: &[Timestamp],
        as_of: Option<i64>,
    ) -> Vec<(Timestamp, Timestamp, V)>
    where
        V: PartialEq,
    {
        if start >= end {
            return Vec::new();
        }
        let Ok(inner) = self.inner.read() else {
            return Vec::new();
        };
        let epoch = *inner.lens_epoch.get(lens).unwrap_or(&0);
        let mut combined: Vec<Layer<V>> = Vec::new();
        for run in inner.runs.iter() {
            let Some(stats) = run.stats.get(lens) else {
                continue;
            };
            if stats.count == 0 || stats.max_end <= start || stats.min_start >= end {
                continue;
            }
            if let Ok(layers) = Self::run_layers_for(run, self.key, lens, epoch) {
                combined.extend(layers);
            }
        }
        if let Some(mem) = inner.memtable.get(lens) {
            combined.extend(mem.iter().cloned());
        }
        scan_layers(&combined, start, end, fixed, as_of)
    }

    fn layers(&self, lens: &str) -> Option<Arc<[Layer<V>]>> {
        let inner = self.inner.read().ok()?;
        if !inner.live_lenses.contains(lens) {
            return None;
        }
        let epoch = *inner.lens_epoch.get(lens).unwrap_or(&0);
        let mut out: Vec<Layer<V>> = Vec::new();
        for run in inner.runs.iter() {
            if let Ok(layers) = Self::run_layers_for(run, self.key, lens, epoch) {
                out.extend(layers);
            }
        }
        if let Some(mem) = inner.memtable.get(lens) {
            out.extend(mem.iter().cloned());
        }
        Some(Arc::from(out))
    }

    fn drop_lens(&mut self, lens: &str) {
        let Ok(mut inner) = self.inner.write() else {
            return;
        };
        inner.memtable.remove(lens);
        inner.live_lenses.remove(lens);
        inner.layer_count.remove(lens);
        self.lens_compact.remove(lens);
        *inner.lens_epoch.entry(lens.to_string()).or_insert(0) += 1;
        let _ = self.persist_manifest_locked(&inner);
    }

    fn lens_names(&self) -> Vec<String> {
        self.inner
            .read()
            .map(|g| g.live_lenses.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn append_schema(&mut self, stmt: &str) -> io::Result<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| io::Error::other("sstable lock poisoned"))?;
        inner.schema.push(stmt.to_string());
        self.persist_manifest_locked(&inner)
    }

    fn schema_stmts(&self) -> Vec<String> {
        self.inner
            .read()
            .map(|g| g.schema.clone())
            .unwrap_or_default()
    }

    fn checkpoint_flush(&self) -> io::Result<bool> {
        let mut inner = self
            .inner
            .write()
            .map_err(|_| io::Error::other("sstable lock poisoned"))?;
        self.flush_memtable_to_run(&mut inner)?;
        self.persist_manifest_locked(&inner)?;
        Ok(true)
    }

    fn set_lens_compact_threshold(&mut self, lens: &str, threshold: Option<usize>) {
        match threshold {
            Some(n) => self.lens_compact.insert(lens.to_string(), n),
            None => self.lens_compact.remove(lens),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tau;
    use crate::services::store::InMemory;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;

    fn tmp_base(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tau-sstable-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn layer(id: u64, items: &[(i64, i64, i32)], written_at: i64) -> Layer<i32> {
        Layer::new_at(
            id,
            items.iter().map(|&(s, e, v)| Tau::new(s, e, v)).collect(),
            written_at,
        )
    }

    #[test]
    fn round_trips_through_memtable_only() {
        let base = tmp_base("mem-only");
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        store.append("x", layer(1, &[(0, 10, 42)], 100)).unwrap();
        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.get("x", &[5], None), Some(42));
        assert_eq!(store.scan("x", 0, 10, &[], None), vec![(0, 10, 42)]);
    }

    #[test]
    fn round_trips_through_a_flushed_run() {
        let base = tmp_base("flush");
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        store.append("x", layer(1, &[(0, 10, 42)], 100)).unwrap();
        assert!(store.checkpoint_flush().unwrap());
        // memtable is now empty; the value must come from the on-disk run.
        assert_eq!(store.at("x", 5), Some(42));
        assert_eq!(store.scan("x", 0, 10, &[], None), vec![(0, 10, 42)]);

        // Reopen from scratch: manifest + run files must reconstruct state.
        drop(store);
        let reopened: Sstable<i32> = Sstable::open(&base, None).unwrap();
        assert_eq!(reopened.at("x", 5), Some(42));
        assert_eq!(reopened.lens_names(), vec!["x".to_string()]);
    }

    #[test]
    fn newest_wins_across_memtable_and_run() {
        let base = tmp_base("newest-wins");
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        store.append("x", layer(1, &[(0, 10, 1)], 100)).unwrap();
        store.checkpoint_flush().unwrap(); // flush layer 1 to a run
        store.append("x", layer(2, &[(0, 10, 2)], 200)).unwrap(); // stays in memtable
        assert_eq!(store.at("x", 5), Some(2));

        // AS OF before the second write still sees the first.
        assert_eq!(store.get("x", &[5], Some(150)), Some(1));
        assert_eq!(store.get("x", &[5], Some(250)), Some(2));
    }

    #[test]
    fn drop_then_recreate_hides_old_epoch_data() {
        let base = tmp_base("epoch");
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        store.append("x", layer(1, &[(0, 10, 1)], 100)).unwrap();
        store.checkpoint_flush().unwrap();
        assert_eq!(store.at("x", 5), Some(1));

        store.drop_lens("x");
        assert_eq!(store.at("x", 5), None);
        assert!(store.layers("x").is_none());

        // Recreate: new data must not resurrect the pre-drop value, even
        // though the old run file (still on disk) contains it.
        store.append("x", layer(1, &[(0, 10, 99)], 300)).unwrap();
        assert_eq!(store.at("x", 5), Some(99));
        assert_eq!(store.at("x", 20), None);
    }

    #[test]
    fn run_gc_preserves_generations_and_values() {
        let base = tmp_base("run-gc");
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        store.set_run_gc_threshold(2);
        for i in 0..5i64 {
            store
                .append(
                    "x",
                    layer(i as u64 + 1, &[(i * 10, i * 10 + 10, i as i32)], i * 100),
                )
                .unwrap();
            store.checkpoint_flush().unwrap(); // one run per append -> forces compaction
        }
        // Run compaction should have merged runs down to a small number.
        let run_count = store.inner.read().unwrap().runs.len();
        assert!(
            run_count <= 2,
            "expected compaction to shrink run count, got {run_count}"
        );
        // Every value must still resolve correctly after compaction.
        for i in 0..5i64 {
            assert_eq!(store.at("x", i * 10 + 5), Some(i as i32));
        }
        // Every transaction-time generation must have survived (HISTORY-equivalent).
        let layers = store.layers("x").unwrap();
        let mut gens: Vec<i64> = layers.iter().map(|l| l.written_at).collect();
        gens.sort_unstable();
        gens.dedup();
        assert_eq!(gens, vec![0, 100, 200, 300, 400]);
    }

    #[test]
    fn encrypted_round_trip() {
        let base = tmp_base("enc");
        let key = [7u8; 32];
        let mut store: Sstable<i32> = Sstable::open(&base, Some(key)).unwrap();
        store.append("x", layer(1, &[(0, 10, 42)], 100)).unwrap();
        store.checkpoint_flush().unwrap();
        drop(store);

        let reopened: Sstable<i32> = Sstable::open(&base, Some(key)).unwrap();
        assert_eq!(reopened.at("x", 5), Some(42));

        // Wrong key must fail to open (footer/manifest decrypt error).
        let wrong = Sstable::<i32>::open(&base, Some([9u8; 32]));
        assert!(wrong.is_err());
    }

    #[test]
    fn nd_round_trip_through_run() {
        let base = tmp_base("nd");
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        let taus = vec![
            Tau::try_new_nd(&[(0, 100), (0, 50)], 1i32).unwrap(),
            Tau::try_new_nd(&[(0, 100), (50, 100)], 2i32).unwrap(),
        ];
        let l = Layer::try_new_nd_at(1, taus, 100).unwrap();
        store.append("grid", l).unwrap();
        store.checkpoint_flush().unwrap();
        assert_eq!(store.get("grid", &[10, 25], None), Some(1));
        assert_eq!(store.get("grid", &[10, 75], None), Some(2));
        assert_eq!(store.scan("grid", 0, 100, &[25], None), vec![(0, 100, 1)]);
    }

    #[hegel::test]
    fn pbt_append_at_matches_reference_across_flushes(tc: TestCase) {
        let base = tmp_base(&format!("pbt-{}", tc.draw(gs::integers::<u64>())));
        let mut store: Sstable<i32> = Sstable::open(&base, None).unwrap();
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(10));
        let mut reference: Vec<(i64, i64, i32, i64)> = Vec::new(); // (start,end,value,written_at)
        let mut cursor: i64 = 0;
        for i in 0..n {
            let gap = tc.draw(gs::integers::<i64>().min_value(0).max_value(20));
            let width = tc.draw(gs::integers::<i64>().min_value(1).max_value(50));
            let value = tc.draw(gs::integers::<i32>());
            let start = cursor + gap;
            let end = start + width;
            cursor = end;
            let written_at = (i as i64) * 100;
            store
                .append("x", layer(i as u64 + 1, &[(start, end, value)], written_at))
                .unwrap();
            reference.push((start, end, value, written_at));
            if tc.draw(gs::booleans()) {
                store.checkpoint_flush().unwrap();
            }
        }
        let probe = tc.draw(gs::integers::<i64>().min_value(-10).max_value(cursor + 10));
        let expected = reference
            .iter()
            .rev()
            .find(|&&(s, e, _, _)| s <= probe && probe < e)
            .map(|&(_, _, v, _)| v);
        assert_eq!(store.at("x", probe), expected);
    }

    /// Un-merged, boundary-decomposed sum over `[start, end)` — mirrors
    /// `query::collect_agg_segments`/`agg_sum`'s "no adjacent-equal-value
    /// merge" semantics, which is what makes `REDUCE … USING sum`/`avg`
    /// directly sensitive to how many physical layers survive compaction
    /// (unlike `AT`/`RANGE`/`AS OF`, which only care about the newest-wins
    /// *result*). Used below to compare `Sstable` against `InMemory` — the
    /// single-unified-stack reference — without needing the full executor/
    /// TauQL/wall-clock stack.
    fn raw_reduce_sum(layers: &[Layer<i32>], start: i64, end: i64) -> i64 {
        if start >= end {
            return 0;
        }
        let mut bounds = vec![start, end];
        for l in layers {
            for t in l.taus.iter() {
                if t.start() > start && t.start() < end {
                    bounds.push(t.start());
                }
                if t.end() > start && t.end() < end {
                    bounds.push(t.end());
                }
            }
        }
        bounds.sort_unstable();
        bounds.dedup();
        bounds
            .windows(2)
            .filter_map(|w| layers_get(layers, &[w[0]], None))
            .map(i64::from)
            .sum()
    }

    /// `Sstable` must match `InMemory` (the single-unified-stack reference)
    /// on `REDUCE`'s raw, unmerged segment sum after *every* op — not just
    /// once state has settled — across randomised appends whose `written_at`
    /// is drawn from a small pool so appends sometimes share a generation
    /// (exercising same-generation merges) and sometimes don't (exercising
    /// preserved-generation boundaries), interleaved with random
    /// `checkpoint_flush` calls that force memtable/run-boundary crossings at
    /// arbitrary points relative to `compact_threshold`. This is the direct
    /// regression test for the class of bug the "one authoritative
    /// consolidation trigger" design (see the module doc) exists to prevent:
    /// any gap that leaves a lens's data fragmented across memtable/runs
    /// shows up here as a sum mismatch, immediately, without needing DST.
    #[hegel::test]
    fn pbt_sstable_matches_inmemory_reduce_sum(tc: TestCase) {
        let base = tmp_base(&format!("cmp-{}", tc.draw(gs::integers::<u64>())));
        let compact_threshold = tc.draw(gs::integers::<usize>().min_value(1).max_value(4));
        let mut sst: Sstable<i32> = Sstable::open(&base, None).unwrap();
        sst.set_compact_threshold(compact_threshold);
        let mut mem: InMemory<i32> = InMemory::with_threshold(compact_threshold);

        let gens = [0i64, 100, 100, 200, 300, 300, 300];
        let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(30));
        let mut cursor: i64 = 0;
        for i in 0..n {
            let gap = tc.draw(gs::integers::<i64>().min_value(0).max_value(15));
            let width = tc.draw(gs::integers::<i64>().min_value(1).max_value(30));
            let value = tc.draw(gs::integers::<i32>());
            let start = cursor + gap;
            let end = start + width;
            cursor = end;
            let gi = tc.draw(
                gs::integers::<usize>()
                    .min_value(0)
                    .max_value(gens.len() - 1),
            );
            let written_at = gens[gi];
            let id = i as u64 + 1;
            sst.append("x", layer(id, &[(start, end, value)], written_at))
                .unwrap();
            mem.append("x", layer(id, &[(start, end, value)], written_at))
                .unwrap();
            if tc.draw(gs::booleans()) {
                sst.checkpoint_flush().unwrap();
            }

            let probe_end = cursor.max(1);
            let sst_layers = sst.layers("x").unwrap();
            let mem_layers = mem.layers("x").unwrap();
            let sst_sum = raw_reduce_sum(&sst_layers, 0, probe_end);
            let mem_sum = raw_reduce_sum(&mem_layers, 0, probe_end);
            assert_eq!(
                sst_sum, mem_sum,
                "op {i}: REDUCE x 0 {probe_end} USING sum diverged (sstable={sst_sum} inmemory={mem_sum})"
            );
        }
    }
}
