# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=bxrne_tau&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=bxrne_tau)

A time-series database built on immutable, layered temporal intervals.

Data is never corrected in place. When a value changes, a new layer is appended on top of existing ones and the newest layer wins at query time. This gives you the full correction history for free and eliminates write-write conflicts entirely.

## Quick start

```bash
cargo run --release                           # in-memory, listens on 127.0.0.1:7070
cargo run --release -- --wal -w data.wal     # with WAL durability
```

Connect with any TCP client:

```
> CREATE DATABASE main
< OK
> CREATE LENS temp float
< OK
> APPEND LENS temp 0 50 18.5, 50 100 21.0
< OK
> AT LENS temp 25
< VAL f18.5
> RANGE LENS temp 0 100
< RANGE 2; 0:50:f18.5; 50:100:f21
> REDUCE LENS temp 0 100 USING avg
< VAL f19.75
> SHOW LENSES
< NAMES 1; temp
```

## Query language

```
databases and lenses
CREATE DATABASE <name>
DROP DATABASE <name>
USE DATABASE <name>
SHOW DATABASES
SHOW LENSES

CREATE LENS <name> <type>                           int | float | str | bool | bytes
APPEND LENS <name> <s> <e> <v> [, <s> <e> <v> ...]
COPY   LENS <name> FROM "<path>"                    server-side CSV ingest (start,end,value)
DERIVE LENS <name> AS <expr>
AT     LENS <name> <timestamp>
RANGE  LENS <name> <start> <end> [WHERE <expr>]
REDUCE LENS <name> <start> <end> USING <func>       min | max | avg | sum | count
DROP   LENS <name>

multi-user auth (requires --auth)
CREATE USER <name> PASSWORD "<pass>"
DROP   USER <name>
GRANT  <perms> ON <db|*> TO   <user>                perms = any of CRUDA, * (all), - (none)
REVOKE <perms> ON <db|*> FROM <user>
SHOW   USERS
SHOW   GRANTS [<user>]
```

Expressions support `+ - * / %`, `== != < <= > >=`, `&& ||`, unary `- !`, and rolling aggregation calls (`avg(lens, rel_start, rel_end)` etc.). Keywords are case-insensitive.

See [`src/libtau/ql/`](src/libtau/ql/README.md) for grammar details and [`src/bin/tauctl/`](src/bin/tauctl/README.md) for a full statement reference.

## Security

Three independent layers, all opt-in.

**TLS:**
```bash
cargo run --release -- --tls                                        # ephemeral self-signed (dev only)
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

**Authentication:**
```bash
cargo run --release -- --auth --username admin --password s3cr3t
cargo run --release -- --auth --users-file /var/lib/tau/users \
  --username admin --password s3cr3t
```

With `--auth`, every client must send `AUTH <user> <pass>` as its first message. Passwords are hashed with argon2id. Permissions are a per-database CRUDA bitmap; `A` on `"*"` is global admin.

**Encryption at rest:**
```bash
TAU_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  cargo run --release -- --wal -w data.wal
```

Set `TAU_ENCRYPTION_KEY` to a 64-hex string (32 bytes). WAL entries are AES-256-GCM encrypted with a random 12-byte nonce per entry.

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
      --password <PASS>                Bootstrap admin password (argon2id-hashed at startup)
      --users-file <PATH>              Persistent multi-user store
      --metrics-port <PORT>            Expose Prometheus /metrics on this HTTP port
      --max-connections <N>            Maximum concurrent client connections [default: 1024]
      --idle-timeout-secs <SECS>       Per-connection idle timeout [default: 300; 0 disables]
  -h, --help                           Print help
  -V, --version                        Print version

Environment:
  TAU_ENCRYPTION_KEY   64 hex chars - enables AES-256-GCM encryption at rest
```

See [`src/bin/tau/`](src/bin/tau/README.md) for concurrency model, TLS, and connection handling details.

## Wire protocol

One statement per line in, one response line out.

| Response | Meaning |
|----------|---------|
| `OK` | DDL or write succeeded |
| `VAL <v>` | Point lookup value; `VAL NIL` when no tau covers the timestamp |
| `RANGE <n>; <s>:<e>:<v> ...` | Range scan, `n` segments |
| `NAMES <n>; name ...` | Name list from `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS` |
| `GRANTS <n>; <user> <db>:<perms> ...; ...` | Result of `SHOW GRANTS` |
| `ERR <message>` | Parse, executor, or permission error |

Values encode as `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

## CSV ingest

Two paths depending on where the file lives:

```bash
# Server-side: file is on the server's filesystem.
COPY LENS cpu FROM "/data/cpu-load.csv"

# Client-side: file lives on your machine - use tauctl's load command.
τ: load cpu examples/data/cpu-load.csv
```

[`examples/`](examples/README.md) walks through committed sample datasets and the canonical queries against them.

## Metrics

When started with `--metrics-port <PORT>`:

```
GET http://127.0.0.1:<PORT>/metrics    Prometheus text-format metrics
GET http://127.0.0.1:<PORT>/healthz    Liveness probe
```

| metric | type | description |
|--------|------|-------------|
| `tau_statements_total{type=...}` | counter | Statements processed per type |
| `tau_statement_duration_microseconds_bucket{type=...,le=...}` | histogram | Latency histogram per statement type |
| `tau_connections_total` | counter | TCP connections accepted since startup |
| `tau_rejected_connections_total` | counter | Connections refused at the `--max-connections` cap |
| `tau_auth_attempts_total` | counter | AUTH messages received |
| `tau_auth_failures_total` | counter | Failed AUTH attempts |
| `tau_errors_total` | counter | ERR responses sent to clients |
| `tau_process_resident_bytes` | gauge | Resident memory (Linux: VmRSS) |
| `tau_process_open_fds` | gauge | Open file descriptors |
| `tau_process_uptime_seconds` | gauge | Seconds since startup |

## Library usage

The executor can be embedded without the TCP server:

```rust
use tau::{Executor, Output, Value, parse};

let mut e = Executor::new();

for q in [
    "CREATE DATABASE main",
    "CREATE LENS celsius float",
    "APPEND LENS celsius 0 50 18.0, 50 100 22.0",
    "DERIVE LENS f AS celsius * 9.0 / 5.0 + 32.0",
    "DERIVE LENS smooth AS avg(celsius, -20, 0)",
] {
    e.exec(&parse(q).unwrap().1).unwrap();
}

let (_, stmt) = parse("AT LENS f 25").unwrap();
assert_eq!(e.exec(&stmt).unwrap(), Output::Value(Some(Value::Float(64.4))));
```

Auth is a server concern; embedded callers use `exec` / `exec_read` directly and bypass permission checks entirely. See [`src/libtau/`](src/libtau/README.md) for design details.

## Testing

Three complementary layers - see [`TEST.md`](TEST.md) for the full design.

| Layer | What it catches | How to run |
|-------|----------------|------------|
| Unit tests | Regressions on known-shape behaviour | `cargo nextest run` |
| Hegel PBT | Invariant violations across random inputs | `cargo nextest run` |
| DST | Emergent correctness bugs across simulated centuries | `cargo run --release --bin dst -- --quick` |

## Container

The release workflow publishes a musl static image to GHCR on every release:

```sh
docker pull ghcr.io/bxrne/tau:latest

cd container
cp .env.example .env        # set TAU_PASSWORD and GRAFANA_PASSWORD at minimum
docker compose up -d        # tau + prometheus + grafana
```

See [`container/README.md`](container/README.md) for the full environment variable reference, TLS setup, Prometheus alert rules, and production hardening checklist.

## Architecture

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the data model (Tau, Layer, Lens), storage backends, WAL, compaction algorithm, and design decisions.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for machine setup, development workflow, and how to add a new statement.

## Roadmap

See [`ROADMAP.md`](ROADMAP.md) for the v0.1.0 status and v1.0 quality criteria.

## License

Tau is distributed under the [PolyForm Noncommercial License 1.0.0](LICENSE).

Permitted: personal use, research, education, charitable organisations, public institutions, and self-hosting for any of the above.

Not permitted without a separate commercial licence: use as part of a paid product, a revenue-generating service, or internal production workloads at a for-profit company. To enquire about a commercial licence, open an issue or contact the repository owner.
