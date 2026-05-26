//! Deterministic benchmark runner for the tau database.
//!
//! Sweeps a grid of (backend × wal × compact_threshold × scale) configurations,
//! measures append and point-lookup throughput, and emits a CSV row per cell so
//! results can be diffed across optimization attempts.
//!
//! Workload generation is fully deterministic: a fixed-seed xorshift PRNG
//! produces the same lens names, timestamps, and values on every run. Each
//! configuration is run in its own tempdir; nothing leaks between cells.
//!
//! Usage:
//!   bench [--label NAME] [--out PATH] [--quick]
//!
//! --quick drops the large scale tiers (useful while iterating). --label is
//! attached to every emitted row so multiple runs can be merged.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tau::{Database, Disk, InMemory, Layer, Tau, Wal};

#[derive(Clone, Copy, Debug)]
enum Backend {
    Memory,
    Disk,
}

impl Backend {
    fn label(self) -> &'static str {
        match self {
            Backend::Memory => "memory",
            Backend::Disk => "disk",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Cell {
    backend: Backend,
    wal: bool,
    /// `true` = per-record fsync (the original durability contract).
    /// `false` = relaxed: writes still go to BufWriter, but no flush+sync_data
    /// on the hot path. Equivalent to async / group-commit durability.
    fsync: bool,
    compact_threshold: usize,
    appends: usize,
    lookups: usize,
}

#[derive(Debug)]
struct Result {
    cell: Cell,
    append_ns_per_op: f64,
    lookup_ns_per_op: f64,
    append_ops_per_sec: f64,
    lookup_ops_per_sec: f64,
}

/// Xorshift64 — deterministic, no deps.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_range(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn build_db(cell: &Cell, dir: &Path) -> (Database<i64>, Option<PathBuf>) {
    let db_and_path = match (cell.backend, cell.wal) {
        (Backend::Memory, false) => (
            Database::new(InMemory::<i64>::with_threshold(cell.compact_threshold)),
            None,
        ),
        (Backend::Memory, true) => {
            let wal_path = dir.join("bench.wal");
            let store = InMemory::<i64>::with_threshold(cell.compact_threshold);
            let mut wal = Wal::open(&wal_path, None).unwrap();
            wal.set_fsync_each(cell.fsync);
            (Database::with_wal(store, wal), Some(wal_path))
        }
        (Backend::Disk, false) => {
            let disk_path = dir.join("bench.dat");
            let mut store = Disk::<i64>::create(&disk_path, None).unwrap();
            store.set_fsync_each(cell.fsync);
            if !cell.fsync {
                store.set_rewrite_on_compact(false);
            }
            (Database::new(store), Some(disk_path))
        }
        (Backend::Disk, true) => {
            let disk_path = dir.join("bench.dat");
            let wal_path = dir.join("bench.wal");
            let mut store = Disk::<i64>::create(&disk_path, None).unwrap();
            store.set_fsync_each(cell.fsync);
            if !cell.fsync {
                store.set_rewrite_on_compact(false);
            }
            let mut wal = Wal::open(&wal_path, None).unwrap();
            wal.set_fsync_each(cell.fsync);
            (Database::with_wal(store, wal), Some(wal_path))
        }
    };
    // In fast mode, also defer WAL checkpointing.
    if !cell.fsync {
        db_and_path.0.set_auto_checkpoint(false);
    }
    db_and_path
}

/// Build the deterministic append workload up-front so timing only measures
/// the database hot path, not the RNG or layer construction.
fn gen_workload(cell: &Cell) -> Vec<(u64, Layer<i64>)> {
    let mut rng = Rng::new(0xC0FFEE_u64.wrapping_add(cell.appends as u64));
    (0..cell.appends)
        .map(|i| {
            let lens_idx = rng.next_range(8); // 8 distinct lenses
            let start = (i as i64) * 10;
            let end = start + 5;
            let val = rng.next_u64() as i64;
            let layer = Layer::new(i as u64 + 1, vec![Tau::new(start, end, val)]);
            (lens_idx, layer)
        })
        .collect()
}

fn gen_lookup_queries(cell: &Cell) -> Vec<(u64, i64)> {
    let mut rng = Rng::new(0xBADF00D_u64.wrapping_add(cell.lookups as u64));
    let max_t = (cell.appends as i64) * 10;
    (0..cell.lookups)
        .map(|_| {
            let lens_idx = rng.next_range(8);
            let t = rng.next_range(max_t.max(1) as u64) as i64;
            (lens_idx, t)
        })
        .collect()
}

fn lens_name(idx: u64) -> String {
    format!("s{}", idx)
}

struct ScratchDir(PathBuf);
impl ScratchDir {
    fn new(root: &Path) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("tau-bench-{}-{}-{}", pid, nanos, n));
        fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_cell(cell: Cell, scratch_root: &Path) -> Result {
    let dir = ScratchDir::new(scratch_root);
    let (db, _path) = build_db(&cell, dir.path());

    let workload = gen_workload(&cell);
    let lookups = gen_lookup_queries(&cell);

    // Pre-create lens handles to avoid measuring Arc<str> allocation.
    let lenses: Vec<_> = (0..8u64).map(|i| db.lens(lens_name(i))).collect();

    // Warmup — touch each lens once with a small layer at far-future timestamps
    // so the first append does not pay HashMap rehash cost in the timed loop.
    for (i, l) in lenses.iter().enumerate() {
        let layer = Layer::new(
            (cell.appends + i + 1) as u64,
            vec![Tau::new(
                i64::MAX / 2 + i as i64 * 100,
                i64::MAX / 2 + i as i64 * 100 + 10,
                0,
            )],
        );
        db.append(l, layer).unwrap();
    }

    // ----- Append phase -----
    let t0 = Instant::now();
    for (lens_idx, layer) in workload {
        db.append(&lenses[lens_idx as usize], layer).unwrap();
    }
    let append_elapsed = t0.elapsed();

    // ----- Lookup phase -----
    let t1 = Instant::now();
    let mut sink: u64 = 0;
    for (lens_idx, t) in lookups {
        if let Some(v) = db.at(&lenses[lens_idx as usize], t) {
            sink = sink.wrapping_add(v as u64);
        }
    }
    let lookup_elapsed = t1.elapsed();

    // Defeat dead-code elim.
    std::hint::black_box(sink);

    let append_ns = append_elapsed.as_nanos() as f64 / cell.appends as f64;
    let lookup_ns = lookup_elapsed.as_nanos() as f64 / cell.lookups as f64;

    Result {
        cell,
        append_ns_per_op: append_ns,
        lookup_ns_per_op: lookup_ns,
        append_ops_per_sec: 1e9 / append_ns.max(1e-9),
        lookup_ops_per_sec: 1e9 / lookup_ns.max(1e-9),
    }
}

fn grid(quick: bool, fsync_only: Option<bool>, appends_override: Option<usize>) -> Vec<Cell> {
    let backends = [Backend::Memory, Backend::Disk];
    let wals = [false, true];
    let thresholds: &[usize] = if quick { &[64, 1024] } else { &[8, 64, 1024] };
    let default_scales: &[usize] = if quick {
        &[1_000, 10_000]
    } else {
        &[1_000, 10_000, 100_000]
    };
    let override_buf = appends_override.map(|n| [n]);
    let scales: &[usize] = match &override_buf {
        Some(s) => s,
        None => default_scales,
    };
    let fsyncs: &[bool] = match fsync_only {
        Some(true) => &[true],
        Some(false) => &[false],
        None => &[true, false],
    };

    let mut cells = Vec::new();
    let mut seen: std::collections::HashSet<(&'static str, bool, usize, usize, bool)> =
        std::collections::HashSet::new();
    for &backend in &backends {
        for &wal in &wals {
            for &t in thresholds {
                for &n in scales {
                    for &fsync in fsyncs {
                        // Pure-memory backend with no WAL has nothing to fsync; only
                        // emit one (fsync=true) cell to avoid a useless duplicate.
                        if matches!(backend, Backend::Memory) && !wal && !fsync {
                            continue;
                        }
                        // On real disk, fsync=true cells are ~50 ops/sec; even 1k
                        // appends takes 20s. Cap fsync=true at the smallest scale
                        // unless the user explicitly overrode appends.
                        let appends = if appends_override.is_none() && fsync && n > 1_000 {
                            1_000
                        } else {
                            n
                        };
                        let key = (backend.label(), wal, t, appends, fsync);
                        if seen.insert(key) {
                            cells.push(Cell {
                                backend,
                                wal,
                                fsync,
                                compact_threshold: t,
                                appends,
                                lookups: 10_000,
                            });
                        }
                    }
                }
            }
        }
    }
    cells
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let label = arg_value(&args, "--label").unwrap_or_else(|| "run".to_string());
    let out = arg_value(&args, "--out");
    let quick = args.iter().any(|a| a == "--quick");
    let only_backend = arg_value(&args, "--backend");

    let fsync_only = arg_value(&args, "--fsync").map(|s| s == "true" || s == "on" || s == "1");
    let scratch_root: PathBuf = arg_value(&args, "--scratch")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&scratch_root).expect("scratch root");

    let appends_override = arg_value(&args, "--appends").and_then(|s| s.parse().ok());

    let cells = grid(quick, fsync_only, appends_override)
        .into_iter()
        .filter(|c| match only_backend.as_deref() {
            None => true,
            Some("memory") => matches!(c.backend, Backend::Memory),
            Some("disk") => matches!(c.backend, Backend::Disk),
            _ => true,
        })
        .collect::<Vec<_>>();

    println!("label={} cells={} (deterministic seed)", label, cells.len());
    println!(
        "{:<8} {:<5} {:<6} {:<10} {:<10} {:<14} {:<14} {:<16} {:<16}",
        "backend",
        "wal",
        "fsync",
        "compact_t",
        "appends",
        "append_ns/op",
        "lookup_ns/op",
        "append_ops/s",
        "lookup_ops/s"
    );

    let mut rows = Vec::new();
    for cell in cells {
        let r = run_cell(cell, &scratch_root);
        println!(
            "{:<8} {:<5} {:<6} {:<10} {:<10} {:<14.1} {:<14.1} {:<16.0} {:<16.0}",
            r.cell.backend.label(),
            r.cell.wal,
            r.cell.fsync,
            r.cell.compact_threshold,
            r.cell.appends,
            r.append_ns_per_op,
            r.lookup_ns_per_op,
            r.append_ops_per_sec,
            r.lookup_ops_per_sec
        );
        rows.push(r);
    }

    if let Some(path) = out {
        let mut f = fs::File::create(&path).expect("open csv");
        writeln!(
            f,
            "label,backend,wal,fsync,compact_threshold,appends,lookups,append_ns_per_op,lookup_ns_per_op,append_ops_per_sec,lookup_ops_per_sec"
        )
        .unwrap();
        for r in &rows {
            writeln!(
                f,
                "{},{},{},{},{},{},{},{:.3},{:.3},{:.1},{:.1}",
                label,
                r.cell.backend.label(),
                r.cell.wal,
                r.cell.fsync,
                r.cell.compact_threshold,
                r.cell.appends,
                r.cell.lookups,
                r.append_ns_per_op,
                r.lookup_ns_per_op,
                r.append_ops_per_sec,
                r.lookup_ops_per_sec
            )
            .unwrap();
        }
        println!("wrote {}", path);
    }
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{}=", flag)) {
            return Some(rest.to_string());
        }
    }
    None
}
