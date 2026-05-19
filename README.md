# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)

A time-series database built on immutable, layered temporal intervals.

Data is never corrected in place. When a value changes, a new layer is appended on top of existing ones and the newest layer wins at query time. This gives you the full correction history for free and eliminates write-write conflicts entirely.

---

## Quick start

```bash
cargo run --release                           # in-memory, listens on 127.0.0.1:7070
cargo run --release -- --wal -w data.wal     # with WAL durability
```

Connect with any TCP client:

```
→ CREATE DATABASE main
← OK
→ CREATE LENS temp float
← OK
→ APPEND LENS temp 0 100 18.5
← OK
→ AT LENS temp 50
← VAL f18.5
→ RANGE LENS temp 0 100
← RANGE 1; 0:100:f18.5
→ REDUCE LENS temp 0 100 USING avg
← VAL f18.5
```

---

## Core concepts

Three primitive types form the whole model:

- **`Tau<V>`** — a value `V` that holds over the half-open interval `[start, end)`. Immutable once created.
- **`Layer<V>`** — a sorted, non-overlapping batch of taus with O(log n) point lookup. Cheaply clonable via `Arc`.
- **`Lens<V>`** — either `Base` (storage-backed, newest layer wins) or `Derived` (a lazy expression over other lenses).

Layers auto-compact: once a lens accumulates more than `--compact-threshold` layers (default 8), they are merged into a single equivalent layer. Point-lookup cost stays at O(log n) regardless of write history.

See [`src/libtau/`](src/libtau/README.md) for design decisions and module layout.

---

## Query language

```
CREATE DATABASE <name>                              -- first created becomes active
DROP DATABASE <name>
USE DATABASE <name>

CREATE LENS <name> <type>                           -- int | float | str | bool | bytes
APPEND LENS <name> <start> <end> <value>
DERIVE LENS <name> AS <expr>
AT     LENS <name> <timestamp>
RANGE  LENS <name> <start> <end> [WHERE <expr>]
REDUCE LENS <name> <start> <end> USING <func>       -- min | max | avg | sum | count
DROP   LENS <name>
```

Expressions support `+ - * / %`, `== != < <= > >=`, `&& ||`, unary `- !`, parentheses, and aggregation calls. Keywords are case-insensitive.

### Rolling aggregations

Aggregation functions are first-class expressions, available in `DERIVE` and `WHERE` filters:

```
avg(lens, rel_start, rel_end)
min(lens, rel_start, rel_end)
max(lens, rel_start, rel_end)
sum(lens, rel_start, rel_end)
count(lens, rel_start, rel_end)
```

`rel_start` and `rel_end` are offsets relative to the evaluation timestamp `t`. `avg(temp, -60, 0)` at `t=100` aggregates `temp` over `[40, 100)`. `avg` is time-weighted.

```
DERIVE LENS smooth   AS avg(temp, -60, 0)
DERIVE LENS hot      AS temp > avg(temp, -300, 0)
DERIVE LENS band_hi  AS avg(temp, -60, 60) + 2.0
```

See [`src/libtau/ql/`](src/libtau/ql/README.md) for grammar details and parser design.

---

## Security

Three independent layers, all opt-in.

### Encryption in transit (TLS)

```bash
cargo run --release -- --tls                                        # ephemeral self-signed (dev only)
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

### Authentication

```bash
cargo run --release -- --auth --username admin --password s3cr3t
```

With auth enabled, every client must send `AUTH <user> <pass>` as its first message. The password is hashed with argon2id at startup; the plaintext is not retained.

```
→ AUTH admin s3cr3t
← OK
→ CREATE DATABASE main
← OK
```

### Encryption at rest

Set `TAU_ENCRYPTION_KEY` to a 64-char hex string (32 bytes). WAL entries are then AES-256-GCM encrypted (random 12-byte nonce per entry). Files written with a key cannot be read without it; unencrypted files remain readable when no key is set.

```bash
TAU_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  cargo run --release -- --wal -w data.wal
```

### All three together

```bash
TAU_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  cargo run --release -- \
    --tls --tls-cert server.crt --tls-key server.key \
    --auth --username admin --password s3cr3t \
    --wal -w /var/lib/tau/data.wal
```

---

## Server reference

```
Usage: tau [OPTIONS] [ADDR]

Arguments:
  [ADDR]  TCP address to bind to [default: 127.0.0.1:7070]

Options:
      --wal                            Enable write-ahead logging for durability
  -w, --wal-path <PATH>                Path for WAL file (required if --wal is set)
  -l, --log-level <LOG_LEVEL>          error | warn | info | debug | trace [default: info]
      --compact-threshold <N>          Layers per lens before compaction [default: 8]
      --tls                            Enable TLS
      --tls-cert <PATH>                PEM certificate (omit for ephemeral self-signed)
      --tls-key <PATH>                 PEM private key
      --auth                           Enable authentication
      --username <NAME>                Username (requires --auth)
      --password <PASS>                Password, hashed with argon2id at startup (requires --auth)
  -h, --help                           Print help
  -V, --version                        Print version

Environment:
  TAU_ENCRYPTION_KEY   64 hex chars — enables AES-256-GCM encryption at rest
```

Wire format: one statement per line in, one response per line out. Values encode as `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

See [`src/bin/tau/`](src/bin/tau/README.md) for concurrency model, connection handling, and known limitations.

---

## Storage backends

| Backend | Use case |
|---------|---------|
| `InMemory` | Tests and ephemeral workloads. HashMap-backed, lost on shutdown. |
| `Disk` | Persisted binary file. Plain (`TAU\x01` magic) or AES-256-GCM encrypted (`TAUE` magic). Unencrypted stores use an open append-mode file handle so each write is O(entry); encrypted stores flush atomically. |
| `Wal` | Append-only durability log. Per-line CRC32 (plain) or `E:<base64>` (encrypted). Replayed into a fresh store on startup. `S:` / `SE:` lines persist schema DDL (`CREATE LENS`, `DERIVE LENS`) so declarations survive a restart. After auto-compaction a checkpoint rewrites the WAL to contain only live layers, keeping disk usage bounded. |

See [`src/libtau/storage/`](src/libtau/storage/README.md) for format details, compaction algorithm, and backend tradeoffs.

---

## Library usage

The executor can be embedded directly without the TCP server:

```rust
use tau::{Executor, Output, Value, parse};

let mut e = Executor::new();

for q in [
    "CREATE DATABASE main",
    "CREATE LENS celsius float",
    "APPEND LENS celsius 0 100 18.0",
    "DERIVE LENS f AS celsius * 9.0 / 5.0 + 32.0",
    "DERIVE LENS smooth AS avg(celsius, -20, 0)",
] {
    e.exec(&parse(q).unwrap().1).unwrap();
}

let (_, stmt) = parse("AT LENS f 50").unwrap();
assert_eq!(e.exec(&stmt).unwrap(), Output::Value(Some(Value::Float(64.4))));
```

---

## Development

```bash
cargo build --release
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

Toolchain is pinned to Rust **1.94.1** (edition 2024) via `rust-toolchain.toml`. CI runs fmt → build → clippy → tests.

---

## Roadmap

See [`ROADMAP.md`](ROADMAP.md) for what's done and what remains before 1.0. The major open items are schema persistence across restarts, WAL log rotation, connection limits, and end-to-end integration tests.
