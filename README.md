# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)

A time-series database built on immutable, layered traces.

## Core Concepts

- **Tau<V>**: Value over `[start, end)` interval. Corrections create new layers, never mutations.
- **Layer<V>**: Sorted, non-overlapping taus. O(log n) lookup, O(1) clone.
- **Lens<V>**: Base (layer stack, newest wins) or Derived (lazy expression view).

## Query Language

```sql
CREATE DATABASE <name>           -- registry; first one is active
DROP DATABASE <name>
USE DATABASE <name>

CREATE LENS <name> <type>        -- int | float | str | bool | bytes
APPEND LENS <name> <start> <end> <value>
DERIVE LENS <name> AS <expr>
AT LENS <name> <timestamp>
RANGE LENS <name> <start> <end> [WHERE <expr>]
DROP LENS <name>
```

Expressions: `+ - * / %`, `== != < <= > >=`, `&& ||`, unary `- !`, parentheses.

## Storage Backends

- **InMemory**: Heap-backed layer stack for ephemeral workloads.
- **Disk**: Binary file with header CRC32 + per-entry checksums.
- **WAL**: Write-ahead log for durability; replays on startup.

## Server

```bash
tau                           # 127.0.0.1:7070 (in-memory, no WAL)
tau --wal -w /path/to.wal     # Enable WAL for durability
tau 0.0.0.0:9000              # Custom bind address
tau --help                    # Show all options
```

Wire protocol: line-oriented TCP. Values encode as `i<int>`, `f<float>`, `s<str>`, `b<0|1>`, `n<NIL>`.

## Example

```rust
use tau::{Executor, Output, parse};

let mut e = Executor::new();
for q in [
    "CREATE DATABASE main",
    "CREATE LENS celsius float",
    "APPEND LENS celsius 0 100 18.0",
    "DERIVE LENS f AS celsius * 9.0 / 5.0 + 32.0",
] {
    e.exec(&parse(q).unwrap().1).unwrap();
}

let (_, stmt) = parse("AT LENS f 50").unwrap();
assert_eq!(e.exec(&stmt).unwrap(), Output::Value(Some(Value::Float(64.4))));
```

## Parser

Uses **nom** parser combinators with PEG-style grammar. The query parser handles:

- SQL keywords and identifiers
- Numeric literals (int, float) and strings
- Binary/unary operators with precedence
- WHERE clause expressions for filtering

## Development

```bash
cargo test         # Run tests
cargo run --release  # Run server
```

