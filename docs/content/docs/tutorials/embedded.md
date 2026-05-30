+++
title = "Embedded Tutorial"
date = 2026-05-28
template = "page.html"
+++

Use Tau as a Rust library with no server process, no TCP, and no authentication overhead. This is the fastest way to use Tau and the right choice for applications that own their data directly.

**Prerequisites:** Rust 1.94.1, `Cargo.toml` for your project.

---

## 1. Add the dependency

In your `Cargo.toml`:

```toml
[dependencies]
tau = { git = "https://github.com/bxrne/tau" }
```

---

## 2. Basic usage

```rust
use tau::{Executor, Output, Value, parse};

fn main() {
    let mut executor = Executor::new();

    // Create a database and lens
    for q in [
        "CREATE DATABASE sensors",
        "CREATE LENS temperature float",
    ] {
        executor.exec(&parse(q).unwrap().1).unwrap();
    }

    // Append some temporal intervals
    executor.exec(
        &parse("APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0")
            .unwrap().1
    ).unwrap();

    // Block-syntax bulk ingest: equivalent to APPEND but easier to generate
    executor.exec(
        &parse("BATCH APPEND LENS temperature { 7200 10800 19.0 ; 10800 14400 23.5 }")
            .unwrap().1
    ).unwrap();

    // Point lookup
    let (_, stmt) = parse("AT LENS temperature 1800").unwrap();
    let result = executor.exec(&stmt).unwrap();
    assert_eq!(result, Output::Value(Some(Value::Float(18.5))));

    // Range scan
    let (_, stmt) = parse("RANGE LENS temperature 0 7200").unwrap();
    if let Output::Range(segments) = executor.exec_read(&stmt).unwrap() {
        for (start, end, value) in &segments {
            println!("[{start}, {end}): {value:?}");
        }
    }
}
```

---

## 3. Derived lenses

Derived lenses evaluate lazily on every query; nothing is materialised.

```rust
for q in [
    "DERIVE LENS fahrenheit AS temperature * 9.0 / 5.0 + 32.0",
    "DERIVE LENS hot AS temperature > 20.0",
    "DERIVE LENS smooth AS avg(temperature, -600, 0)",
] {
    executor.exec(&parse(q).unwrap().1).unwrap();
}

let (_, stmt) = parse("AT LENS fahrenheit 1800").unwrap();
let result = executor.exec_read(&stmt).unwrap();
// Value::Float(65.3)
```

---

## 4. Executor configuration

```rust
// Custom compaction threshold (default: 8 layers)
let executor = Executor::with_threshold(4);

// In-memory: all state is lost on drop (default)
// Disk-backed: requires building a Database with a Disk store
```

The `Executor::new()` and `Executor::with_threshold(n)` constructors create in-memory executors. For disk persistence, construct a `Database` with a `Disk` store and a `Wal`, then build an `Executor` around it.

---

## 5. Handling outputs

`exec` and `exec_read` return `Result<Output, ExecError>`. Pattern-match on `Output` to extract results:

```rust
match executor.exec_read(&stmt)? {
    Output::Value(Some(v))     => println!("value: {v:?}"),
    Output::Value(None)        => println!("no value at that timestamp"),
    Output::Range(segs)        => println!("{} segments", segs.len()),
    Output::Names(names)       => println!("names: {names:?}"),
    Output::Empty              => println!("ok (DDL)"),
    Output::Grants(g)          => println!("grants: {g:?}"),
    Output::LayerHistory(layers) => println!("{} layers", layers.len()),
}
```

---

## 6. Layer audit and temporal queries

`HISTORY LENS` returns layer metadata; `AT LENS … AS OF` and `AT LENS … LAYER` provide point-in-time and layer-specific lookups.

```rust
use tau::LayerInfo;

// Audit layers
let (_, stmt) = parse("HISTORY LENS temperature").unwrap();
if let Output::LayerHistory(layers) = executor.exec_read(&stmt).unwrap() {
    for LayerInfo { id, written_at, min_start, max_end, tau_count } in &layers {
        println!("layer {id}: {tau_count} taus [{min_start}, {max_end}) written_at={written_at}");
    }
    // Query a specific layer by its ID
    if let Some(first) = layers.first() {
        let q = format!("AT LENS temperature 1800 LAYER {}", first.id);
        let (_, stmt) = parse(&q).unwrap();
        println!("{:?}", executor.exec_read(&stmt).unwrap());
    }
}

// Time-travel: what did the lens look like at a specific wall-clock time?
let (_, stmt) = parse("AT LENS temperature 1800 AS OF 1717000000000").unwrap();
println!("{:?}", executor.exec_read(&stmt).unwrap());
```

---

## 7. Using exec vs exec_read

Two entry points with the same semantics but different lock behaviour:

- `exec(&stmt)`: takes a write lock; use for DDL and write statements (`APPEND`, `DERIVE`, etc.)
- `exec_read(&stmt)`: takes a read lock; use for query statements (`AT`, `RANGE`, `REDUCE`, `SHOW *`)

Using `exec_read` for queries allows concurrent read operations on a shared executor. Using `exec` serialises all access.

Neither entry point performs any permission check; auth is a server concern. Embedded callers bypass CRUDA entirely.

---

## 8. Concurrent access

Wrap the executor in `Arc<RwLock<Executor>>` for multi-threaded use:

```rust
use std::sync::{Arc, RwLock};
use tau::{Executor, parse};

let executor = Arc::new(RwLock::new(Executor::new()));

// Writer thread
{
    let exec = Arc::clone(&executor);
    std::thread::spawn(move || {
        let mut e = exec.write().unwrap();
        e.exec(&parse("APPEND LENS cpu 0 60 45").unwrap().1).unwrap();
    });
}

// Reader thread (concurrent with other readers)
{
    let exec = Arc::clone(&executor);
    std::thread::spawn(move || {
        let e = exec.read().unwrap();
        let (_, stmt) = parse("AT LENS cpu 30").unwrap();
        let _ = e.exec_read(&stmt);
    });
}
```

---

## 9. WAL durability

```rust
use tau::{Database, Executor, Wal, storage::disk::Disk};

// Build a disk-backed database with a WAL
let db = Database::new(
    Disk::open("/path/to/data.tau")?,
    Some(Wal::open("/path/to/data.wal")?),
);
```

For most embedded use cases, in-memory storage is sufficient. The WAL is primarily for the server use case where durability across restarts is required.

---

## 10. Performance considerations

The embedded executor skips all network overhead. Performance is bounded by:

- **Write path:** WAL fsync (if enabled), in-memory append, optional compaction
- **Read path:** layer scan with skip-check bounds, binary search per layer

For bulk ingestion, use `BATCH APPEND LENS name { s e v ; ... }` or batch multiple intervals in one `APPEND` to create one layer rather than many. This reduces compaction pressure and speeds up subsequent queries.

Disable per-fsync durability for bulk loads:

```rust
wal.set_fsync_each(false);
disk.set_fsync_each(false);
db.set_auto_checkpoint(false);
// ... bulk load ...
// then flush and re-enable for operational use
```

---

## Next steps

- [TauQL Reference](/docs/tauql/): all statement syntax including BATCH APPEND, HISTORY LENS, AT AS OF, BACKUP/RESTORE
- [Permissions](/docs/permissions/): CRUDA bitmap (auth is bypassed in embedded mode but good to know)
- [How it works](/docs/how-it-works/): how the executor and storage layer work
- [Testing](/docs/testing/): using the DST embedded mode for correctness validation
