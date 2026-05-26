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
→ APPEND LENS temp 0 50 18.5, 50 100 21.0
← OK
→ AT LENS temp 25
← VAL f18.5
→ RANGE LENS temp 0 100
← RANGE 2; 0:50:f18.5; 50:100:f21
→ REDUCE LENS temp 0 100 USING avg
← VAL f19.75
→ SHOW LENSES
← NAMES 1; temp
```

---

## Core concepts

Three primitive types form the whole model:

- **`Tau<V>`** - a value `V` that holds over the half-open interval `[start, end)`. Immutable once created.
- **`Layer<V>`** - a sorted, non-overlapping batch of taus with O(log n) point lookup. Cheaply clonable via `Arc`.
- **`Lens<V>`** - either `Base` (storage-backed, newest layer wins) or `Derived` (a lazy expression over other lenses).

Layers auto-compact: once a lens accumulates more than `--compact-threshold` layers (default 8), they are merged into a single equivalent layer. Point-lookup cost stays at O(log n) regardless of write history.

See [`src/libtau/`](src/libtau/README.md) for design decisions and module layout.

---

## Query language

```
-- databases & lenses
CREATE DATABASE <name>                              -- first created becomes active
DROP DATABASE <name>
USE DATABASE <name>
SHOW DATABASES                                      -- list all database names
SHOW LENSES                                         -- list all lens names in active database

CREATE LENS <name> <type>                           -- int | float | str | bool | bytes
APPEND LENS <name> <s> <e> <v> [, <s> <e> <v> …]  -- single or bulk tau write
COPY   LENS <name> FROM "<path>"                    -- ingest from CSV (start,end,value)
DERIVE LENS <name> AS <expr>
AT     LENS <name> <timestamp>
RANGE  LENS <name> <start> <end> [WHERE <expr>]
REDUCE LENS <name> <start> <end> USING <func>       -- min | max | avg | sum | count
DROP   LENS <name>

-- multi-user auth (requires --auth, admin only)
CREATE USER <name> PASSWORD "<pass>"
DROP   USER <name>
GRANT  <perms> ON <db|*> TO   <user>                -- perms = any of CRUDA, or `*` (all), `-` (none)
REVOKE <perms> ON <db|*> FROM <user>
SHOW   USERS
SHOW   GRANTS [<user>]
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

### Authentication & multi-user authorisation

```bash
# bootstrap a single-admin in-memory store
cargo run --release -- --auth --username admin --password s3cr3t

# persistent multi-user store (first run with --username/--password seeds the admin)
cargo run --release -- --auth \
  --users-file /var/lib/tau/users \
  --username admin --password s3cr3t
```

With `--auth`, every client's first message must be `AUTH <user> <pass>`. Passwords are hashed with argon2id; plaintext is never retained.

After authentication every subsequent statement is gated by the matched user's per-database **CRUDA** bitmap:

| bit | grants |
|---|---|
| `C` | `CREATE LENS`, `DERIVE LENS` |
| `R` | `AT`, `RANGE`, `REDUCE`, `SHOW LENSES` |
| `U` | `APPEND LENS`, `COPY LENS` |
| `D` | `DROP LENS` (and `DROP DATABASE` when `A` is also held) |
| `A` | admin - manage users, grant/revoke, create databases |

Grants are per database, plus a wildcard `*` that applies to every database (current and future). Effective permissions are the union of the per-db grant and the wildcard. A user with `A` on `*` is a **global admin** - they can create databases, create/drop users, and grant/revoke on any database. Promoting an existing user to admin is just `GRANT A ON * TO <user>`.

```
→ AUTH admin s3cr3t
← OK
→ CREATE USER alice PASSWORD "p4ss"
← OK
→ GRANT R ON main TO alice
← OK
→ SHOW GRANTS alice
← GRANTS 1; alice main:R
```

When `--users-file` is set, every `CREATE USER` / `DROP USER` / `GRANT` / `REVOKE` is atomically rewritten to the file (each line: `<name> <argon2-hash> <db>:<perms> …`).

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
      --username <NAME>                Bootstrap admin username (requires --auth)
      --password <PASS>                Bootstrap admin password, hashed with argon2id at startup
      --users-file <PATH>              Persistent multi-user store. Seeded by --username/--password
                                       when the file is empty; thereafter the file is source of truth.
  -h, --help                           Print help
  -V, --version                        Print version

Environment:
  TAU_ENCRYPTION_KEY   64 hex chars - enables AES-256-GCM encryption at rest
```

Wire format: one statement per line in, one response per line out.

| Response | Meaning |
|----------|---------|
| `OK` | DDL or write succeeded |
| `VAL <v>` | Point lookup value; `VAL NIL` when no tau covers the timestamp |
| `RANGE <n>; <s>:<e>:<v> …` | Range scan, `n` segments |
| `NAMES <n>; name …` | Name list from `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS` |
| `GRANTS <n>; <user> <db>:<perms> … ; …` | Result of `SHOW GRANTS` |
| `ERR <message>` | Parse, executor, or permission error |

Values encode as `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

See [`src/bin/tau/`](src/bin/tau/README.md) for concurrency model, connection handling, and known limitations. The interactive REPL - [`tauctl`](src/bin/tauctl/README.md) - speaks the same wire protocol, supports TLS, and includes commands for managing multiple connections at once.

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
    "APPEND LENS celsius 0 50 18.0, 50 100 22.0",   // bulk append - one layer
    "DERIVE LENS f AS celsius * 9.0 / 5.0 + 32.0",
    "DERIVE LENS smooth AS avg(celsius, -20, 0)",
] {
    e.exec(&parse(q).unwrap().1).unwrap();
}

let (_, stmt) = parse("AT LENS f 25").unwrap();
assert_eq!(e.exec(&stmt).unwrap(), Output::Value(Some(Value::Float(64.4))));

// Introspect what lenses exist.
let (_, stmt) = parse("SHOW LENSES").unwrap();
let Output::Names(names) = e.exec(&stmt).unwrap() else { panic!() };
assert!(names.contains(&"celsius".to_string()));
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

## Benchmarks

A deterministic grid runner (`src/bin/bench/main.rs`) sweeps `(backend × wal × fsync × compact_threshold × scale)` and emits one CSV row per cell. See [`BENCHMARKS.md`](BENCHMARKS.md) for the methodology and per-attempt notes.

```bash
cargo run --release --bin bench -- --quick                             # fast iteration, tmpfs
cargo run --release --bin bench -- --scratch /var/tmp/tau --out r.csv  # full grid, real disk
```

Fast mode means `Wal::set_fsync_each(false)`, `Disk::set_rewrite_on_compact(false)`, and `Database::set_auto_checkpoint(false)` - opt-in via setters; defaults preserve per-record fsync durability.

## Roadmap

See [`ROADMAP.md`](ROADMAP.md) for what's done and what remains before 1.0. The major open items are connection limits, graceful shutdown, end-to-end integration tests, and the operational tooling suite.
