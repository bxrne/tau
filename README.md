# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)

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

## Core concepts

Three primitive types form the whole model:

- **`Tau<V>`** - a value `V` that holds over the half-open interval `[start, end)`. Immutable once created.
- **`Layer<V>`** - a sorted, non-overlapping batch of taus with O(log n) point lookup. Cheaply clonable via `Arc`.
- **`Lens<V>`** - either `Base` (storage-backed, newest layer wins) or `Derived` (a lazy expression over other lenses).

Layers auto-compact: once a lens accumulates more than `--compact-threshold` layers (default 8), they are merged into a single equivalent layer. Point-lookup cost stays at O(log n) regardless of write history.

See [`src/libtau/`](src/libtau/README.md) for design decisions and trade-offs.

## Query language

```
databases and lenses
CREATE DATABASE <name>                              first created becomes active
DROP DATABASE <name>
USE DATABASE <name>
SHOW DATABASES                                      list all database names
SHOW LENSES                                         list all lens names in active database

CREATE LENS <name> <type>                           int | float | str | bool | bytes
APPEND LENS <name> <s> <e> <v> [, <s> <e> <v> ...]  single or bulk tau write
COPY   LENS <name> FROM "<path>"                    server-side CSV ingest (start,end,value)
DERIVE LENS <name> AS <expr>
AT     LENS <name> <timestamp>
RANGE  LENS <name> <start> <end> [WHERE <expr>]
REDUCE LENS <name> <start> <end> USING <func>       min | max | avg | sum | count
DROP   LENS <name>

multi-user auth (requires --auth, admin only)
CREATE USER <name> PASSWORD "<pass>"
DROP   USER <name>
GRANT  <perms> ON <db|*> TO   <user>                perms = any of CRUDA, or * (all), - (none)
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

## Security

Three independent layers, all opt-in.

### Encryption in transit (TLS)

```bash
cargo run --release -- --tls                                        # ephemeral self-signed (dev only)
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

### Authentication and multi-user authorisation

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
> AUTH admin s3cr3t
< OK
> CREATE USER alice PASSWORD "p4ss"
< OK
> GRANT R ON main TO alice
< OK
> SHOW GRANTS alice
< GRANTS 1; alice main:R
```

When `--users-file` is set, every `CREATE USER` / `DROP USER` / `GRANT` / `REVOKE` is atomically rewritten to the file (each line: `<name> <argon2-hash> <db>:<perms> ...`).

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
      --metrics-port <PORT>            Expose Prometheus /metrics on this HTTP port (optional)
      --max-connections <N>            Maximum concurrent client connections [default: 1024]
      --idle-timeout-secs <SECS>       Per-connection idle timeout in seconds [default: 300; 0 disables]
  -h, --help                           Print help
  -V, --version                        Print version

Environment:
  TAU_ENCRYPTION_KEY   64 hex chars - enables AES-256-GCM encryption at rest
```

### Ingesting CSV

Two paths, picked by where the file lives:

```bash
# Server-side: the file is already on the server's filesystem (embedded mode,
# Docker volume, etc.).  Runs as a normal tauql statement.
COPY LENS temp FROM "/data/temperature.csv"

# Client-side: the file lives on your machine.  Use `tauctl`'s `load`
# command, which reads the file locally and ships it to the active
# connection as batched APPEND statements.  No server-side path access
# required, works through TLS and auth.
τ: load temp examples/data/temperature.csv
```

Sample data lives in [`examples/data/`](examples/data/) - a couple of small
CSVs (`temperature.csv`, `throughput.csv`) sized for documentation and
demos, not benchmarks.

Wire format: one statement per line in, one response per line out.

| Response | Meaning |
|----------|---------|
| `OK` | DDL or write succeeded |
| `VAL <v>` | Point lookup value; `VAL NIL` when no tau covers the timestamp |
| `RANGE <n>; <s>:<e>:<v> ...` | Range scan, `n` segments |
| `NAMES <n>; name ...` | Name list from `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS` |
| `GRANTS <n>; <user> <db>:<perms> ...; ...` | Result of `SHOW GRANTS` |
| `ERR <message>` | Parse, executor, or permission error |

Values encode as `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

See [`src/bin/tau/`](src/bin/tau/README.md) for concurrency model, connection handling, and known limitations. The interactive REPL - [`tauctl`](src/bin/tauctl/README.md) - speaks the same wire protocol, supports TLS, and includes commands for managing multiple connections at once.

## Metrics

When started with `--metrics-port <PORT>`, the server exposes a Prometheus-compatible HTTP endpoint on that port. The same listener also answers `GET /healthz` for liveness probes; every request is logged at `debug` with method, path, status and duration.

```
GET http://127.0.0.1:<PORT>/metrics
GET http://127.0.0.1:<PORT>/healthz
```

The response is `text/plain` in the OpenMetrics exposition format.

| metric | type | description |
|--------|------|-------------|
| `tau_statements_total{type=...}` | counter | Statements processed per type: `append`, `at`, `range`, `reduce`, `ddl` |
| `tau_statement_nanoseconds_total{type=...}` | counter | Cumulative executor time per type, in ns (excludes network/parse) |
| `tau_statement_duration_microseconds_bucket{type=...,le=...}` | histogram | Latency histogram (us) per statement type. Buckets: 1, 5, 10, 25, 50, 100, 250, 500, 1000, 5000, 10000, 50000, 100000, 500000, +Inf |
| `tau_connections_total` | counter | TCP connections accepted since startup |
| `tau_rejected_connections_total` | counter | Connections refused at the accept boundary (`--max-connections` cap) |
| `tau_auth_attempts_total` | counter | AUTH messages received (successful + failed) |
| `tau_auth_failures_total` | counter | Failed AUTH attempts |
| `tau_errors_total` | counter | ERR responses sent to clients |
| `tau_process_resident_bytes` | gauge | Resident memory of the tau process (Linux: `/proc/self/status` VmRSS, 0 elsewhere) |
| `tau_process_virtual_bytes` | gauge | Virtual memory (VmSize) |
| `tau_process_open_fds` | gauge | Open file descriptors (`/proc/self/fd`) |
| `tau_process_threads` | gauge | OS threads in the tau process |
| `tau_process_uptime_seconds` | gauge | Seconds since the metrics subsystem started |

All counters are monotonically increasing. The metrics thread does not affect query serving and prints structured `tracing` events for every request.

## Storage backends

| Backend | Use case |
|---------|---------|
| `InMemory` | Tests and ephemeral workloads. HashMap-backed, lost on shutdown. |
| `Disk` | Persisted binary file. Plain (`TAU\x01` magic) or AES-256-GCM encrypted (`TAUE` magic). Unencrypted stores use an open append-mode file handle so each write is O(entry); encrypted stores flush atomically. |
| `Wal` | Append-only durability log. Per-line CRC32 (plain) or `E:<base64>` (encrypted). Replayed into a fresh store on startup. `S:` / `SE:` lines persist schema DDL (`CREATE LENS`, `DERIVE LENS`) so declarations survive a restart. After auto-compaction a checkpoint rewrites the WAL to contain only live layers, keeping disk usage bounded. |

See [`src/libtau/storage/`](src/libtau/storage/README.md) for format details, compaction algorithm, and backend tradeoffs.

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

## Development

```bash
cargo build --release
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt
```

Toolchain is pinned to Rust **1.94.1** (edition 2024) via `rust-toolchain.toml`. CI runs fmt -> build -> clippy -> tests.

## Benchmarks

The bench binary (`src/bin/bench/`) spawns a real `tau` server process for each configuration cell and measures throughput via the server's Prometheus `/metrics` endpoint. This covers every server dimension: transport (plain/TLS), authentication (none/password), and WAL (off/on).

```bash
cargo run --release --bin bench -- --quick                             # CI-suitable fast grid
cargo run --release --bin bench -- --scratch /var/tmp/tau --out r.csv  # full grid, real disk
```

Bench options:

| flag | default | description |
|------|---------|-------------|
| `--quick` | off | Plain/no-auth only; fewer cells, suitable for CI |
| `--ops N` | 1000 | Operations per workload run |
| `--repeat N` | 3 | Best-of-N runs per cell |
| `--compact-threshold N` | 64 | Compaction threshold per server |
| `--scratch DIR` | $TMPDIR | WAL scratch directory (use a real disk path for fsync timings) |
| `--out PATH` | none | Write CSV results to path |
| `--label NAME` | run | Tag attached to every CSV row |

See [`ROADMAP.md`](ROADMAP.md) for background on the bench design and past optimization work.

## Container usage

The release workflow at [`.github/workflows/release.yml`](.github/workflows/release.yml) publishes a multi-stage musl static image to GitHub Container Registry on every GitHub release. The image is pulled with:

```sh
# Latest release
docker pull ghcr.io/bxrne/tau:latest

# Pin to a specific release tag
docker pull ghcr.io/bxrne/tau:v0.4.0
```

### Standalone container run

```sh
docker run --rm -it \
  --name tau \
  -p 127.0.0.1:7070:7070 \
  -p 127.0.0.1:9100:9100 \
  -v tau_data:/data \
  -e TAU_ENCRYPTION_KEY="$(openssl rand -hex 32)" \
  --read-only --cap-drop=ALL --security-opt=no-new-privileges:true \
  ghcr.io/bxrne/tau:latest \
  0.0.0.0:7070 \
  --wal -w /data/tau.wal \
  --users-file /data/users.json \
  --metrics-port 9100 \
  --auth --username admin --password "$ADMIN_PASSWORD" \
  --max-connections 1024 \
  --idle-timeout-secs 300 \
  --log-level info
```

The image runs as a static `scratch` binary: no shell, no package manager, only `/tau`. The data volume `/data` is the only writable location and holds the WAL, the users database, and (optionally) the encrypted Disk store.

### Production stack (Prometheus + Grafana)

```sh
cd container
cp .env.example .env                # set TAU_PASSWORD, GRAFANA_PASSWORD at minimum
docker compose up -d                # uses the GHCR image; build:context is the fallback
```

The stack provisions:

- `tau` (this server) on `127.0.0.1:7070` (TauQL) and `127.0.0.1:9100` (metrics)
- `prometheus` on `127.0.0.1:9090`, scraping `tau:9100` every 10s
- `grafana` on `127.0.0.1:3000`, pre-loaded with the tau dashboard and alert rules

### Environment variables (compose)

| Variable | Required | Default | Meaning |
|----------|----------|---------|---------|
| `TAU_USERNAME`           | yes | -    | Bootstrap admin username (seeds `users.json` on a fresh volume) |
| `TAU_PASSWORD`           | yes | -    | Bootstrap admin password (argon2id-hashed at startup) |
| `TAU_ENCRYPTION_KEY`     | no  | -    | 64-hex string; enables AES-256-GCM encryption at rest |
| `TAU_IMAGE_TAG`          | no  | `latest` | Pin to a release tag (`v0.4.0`, etc.) |
| `TAU_LOG_LEVEL`          | no  | `info` | `error \| warn \| info \| debug \| trace` |
| `TAU_COMPACT_THRESHOLD`  | no  | `8`  | Layers per lens before auto-compaction |
| `TAU_BIND_ADDR`          | no  | `127.0.0.1` | Host interface for the query port |
| `TAU_METRICS_BIND_ADDR`  | no  | `127.0.0.1` | Host interface for the metrics port |
| `TAU_CPU_LIMIT`          | no  | `2.0` | docker `deploy.resources.limits.cpus` |
| `TAU_MEM_LIMIT`          | no  | `512M` | docker `deploy.resources.limits.memory` |
| `GRAFANA_USER`           | yes | `admin` | Grafana admin login |
| `GRAFANA_PASSWORD`       | yes | -    | Grafana admin password (no default) |
| `PROM_RETENTION`         | no  | `30d` | Prometheus TSDB retention by time |
| `PROM_RETENTION_SIZE`    | no  | `8GB` | Prometheus TSDB retention by size |
| `GF_SERVER_ROOT_URL`     | no  | `http://localhost:3000` | Public URL Grafana renders into alert/share links |

### TLS inside containers

1. Mount real PEM material at a known path:
   ```yaml
   volumes:
     - /etc/tau/tls/server.crt:/data/tls/server.crt:ro
     - /etc/tau/tls/server.key:/data/tls/server.key:ro
   ```
2. Append `--tls --tls-cert /data/tls/server.crt --tls-key /data/tls/server.key` to the `command:` array.
3. Connect with `tauctl`:
   ```sh
   ctl
   > connect prod 127.0.0.1:7070 tls admin "$ADMIN_PASSWORD"
   ```

If the cert is omitted but `--tls` is set, the server generates an ephemeral self-signed cert at boot. Use this only for development and only against clients that explicitly opt out of cert validation.

### Healthcheck

The runtime base image is `scratch` with no shell, `curl` or `wget`, so docker's `HEALTHCHECK` instruction cannot run inside the container. Liveness comes from Prometheus instead: the `TauDown` alert in `container/prometheus/alerts.yml` fires when `up{job="tau"} == 0` for more than a minute. External orchestrators (Kubernetes, Nomad, etc.) should probe `GET /healthz` on the metrics port directly.

### Production hardening checklist

- [ ] Set strong `TAU_PASSWORD` and `GRAFANA_PASSWORD`
- [ ] Set `TAU_ENCRYPTION_KEY` for WAL encryption at rest
- [ ] Mount real TLS certificates and enable `--tls`
- [ ] Put a reverse proxy (nginx/caddy) in front of Grafana with HTTPS
- [ ] Configure an Alertmanager target and on-call routing
- [ ] Tune `TAU_CPU_LIMIT` and `TAU_MEM_LIMIT` for your workload
- [ ] Tune `--max-connections` and `--idle-timeout-secs`
- [ ] Back up the `tau_data` volume on a schedule (it contains the WAL and `users.json`)

## Roadmap

See [`ROADMAP.md`](ROADMAP.md) for what's done and what remains. End-to-end integration tests and the operational tooling suite are the major remaining items.

## License

Tau is distributed under the [PolyForm Noncommercial License 1.0.0](LICENSE).

**Permitted (no payment required):**

- Personal use, hobby projects, research, experimentation, study, and hobby projects.
- Use by charitable organisations, educational institutions, public research bodies, public safety / health agencies, environmental protection organisations, and government institutions.
- Self-hosting Tau for any of the above, including running the Docker image inside your own infrastructure.
- Modifying Tau and distributing the modified source so long as recipients receive the same licence terms.

**Not permitted without a separate commercial licence:**

- Any commercial purpose, including using Tau (or a derivative of it) as part of a paid product, paid service, or revenue-generating business activity.
- Hosting Tau as a managed service that you sell access to.
- Internal use by a for-profit company for production workloads.

If you need a commercial licence, open an issue or get in touch via the email associated with the repository owner. The default position is "no" unless we explicitly agree otherwise in writing.
