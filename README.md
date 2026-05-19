# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)

A time-series database built on immutable, layered temporal intervals.

## Core Concepts

- **`Tau<V>`** — a value `V` that holds over the half-open interval `[start, end)`. Immutable once created; corrections append a new layer rather than mutating existing data.
- **`Layer<V>`** — a sorted, non-overlapping batch of taus. O(log n) point lookup via binary search; cheaply clonable via `Arc`.
- **`Lens<V>`** — either `Base` (backed by a store, newest layer wins) or `Derived` (a lazy expression over other lenses).

Layers auto-compact: once a lens accumulates more than `--compact-threshold` layers (default 8), they are merged into a single canonical layer, keeping point-lookup cost at O(log n) regardless of write history.

## Query Language

```
CREATE DATABASE <name>                              -- first created is active
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

Expressions support `+ - * / %`, `== != < <= > >=`, `&& ||`, unary `- !`, parentheses, and aggregation calls.

### Aggregation in expressions

Aggregation functions are first-class expressions, usable in `DERIVE` and `WHERE`:

```
avg(lens, rel_start, rel_end)
min(lens, rel_start, rel_end)
max(lens, rel_start, rel_end)
sum(lens, rel_start, rel_end)
count(lens, rel_start, rel_end)
```

`rel_start` and `rel_end` are offsets relative to the evaluation timestamp `t`, so `avg(temp, -60, 0)` at `t=100` aggregates `temp` over `[40, 100)`.

```
DERIVE LENS smooth   AS avg(temp, -60, 0)
DERIVE LENS hot      AS temp > avg(temp, -300, 0)
DERIVE LENS band_hi  AS avg(temp, -60, 60) + 2.0
```

`avg` is time-weighted (area under the step function divided by window duration).

## Security

Tau supports three independent security layers, all opt-in:

### Encryption in transit (TLS)

```bash
# Ephemeral self-signed cert — dev only
cargo run --release -- --tls

# Production: provide your own cert and key
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

Clients connect with any TLS-capable tool (e.g. `openssl s_client`).

### Authentication

```bash
cargo run --release -- --auth --username admin --password s3cr3t
```

With auth enabled, every client must send `AUTH <user> <pass>` as its **first** message before any query is accepted. The password is hashed with argon2id at startup and never retained in plaintext.

Wire exchange with auth:
```
→ AUTH admin s3cr3t
← OK
→ CREATE DATABASE main
← OK
```

### Encryption at rest

Set `TAU_ENCRYPTION_KEY` to a 64-char hex string (32 bytes) before starting the server. WAL entries are AES-256-GCM encrypted (random 12-byte nonce per entry); the Disk backend encrypts the whole file on every flush.

```bash
TAU_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  cargo run --release -- --wal -w data.wal
```

Files written with a key cannot be read without it. Unencrypted WAL files remain readable when no key is set (backward compatible).

### Combining all three

```bash
TAU_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  cargo run --release -- \
    --tls --tls-cert server.crt --tls-key server.key \
    --auth --username admin --password s3cr3t \
    --wal -w /var/lib/tau/data.wal
```

## Server

```
A time-series database TCP server

Usage: tau [OPTIONS] [ADDR]

Arguments:
  [ADDR]  TCP address to bind to (host:port) [default: 127.0.0.1:7070]

Options:
      --wal
          Enable write-ahead logging for durability
  -w, --wal-path <PATH>
          Path for WAL file (required if --wal is set)
  -l, --log-level <LOG_LEVEL>
          Log level (error, warn, info, debug, trace) [default: info]
      --compact-threshold <COMPACT_THRESHOLD>
          Number of layers per lens before automatic compaction into one [default: 8]
      --tls
          Enable TLS (encryption in transit)
      --tls-cert <PATH>
          PEM-encoded TLS certificate (omit to generate ephemeral self-signed)
      --tls-key <PATH>
          PEM-encoded TLS private key
      --auth
          Enable username/password authentication
      --username <NAME>
          Username (requires --auth)
      --password <PASS>
          Password hashed with argon2id at startup (requires --auth)
  -h, --help
          Print help
  -V, --version
          Print version

Environment:
  TAU_ENCRYPTION_KEY   64 hex chars (32 bytes) — enables AES-256-GCM encryption at rest
```

Wire protocol: line-oriented TCP — one statement per line in, one response line out.

```
→ AUTH admin s3cr3t        (only if --auth is set)
← OK
→ CREATE DATABASE main
← OK
→ APPEND LENS temp 0 100 18.5
← OK
→ AT LENS temp 50
← VAL f18.5
→ RANGE LENS temp 0 100
← RANGE 1; 0:100:f18.5
→ REDUCE LENS temp 0 100 USING avg
← VAL f18.5
→ BOGUS
← ERR parse: ...
```

Values encode as `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

## Storage Backends

- **`InMemory`** — heap-backed `HashMap`, suitable for tests and ephemeral workloads.
- **`Disk`** — binary file; plain (`TAU\x01` magic) or AES-256-GCM encrypted (`TAUE` magic). Per-entry CRC32 checksums when unencrypted; AEAD authentication tag when encrypted.
- **`Wal`** — append-only flat file; each line is either `<crc32> <layer_id> <lens> <start>:<end>:<value> …` (plain) or `E:<base64(nonce||ciphertext)>` (encrypted). Replayed into a fresh store on startup.

## Library Usage

```rust
use tau::{Executor, Output, Value, parse};

let mut e = Executor::new();                         // default compact threshold
let mut e = Executor::with_threshold(32);            // custom threshold

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

let (_, stmt) = parse("REDUCE LENS celsius 0 100 USING avg").unwrap();
// returns Output::Value(Some(Value::Float(18.0)))
```

## Development

```bash
cargo build --release
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
cargo run --release                                  # TCP server
cargo run --release -- --compact-threshold 32        # custom threshold
```
