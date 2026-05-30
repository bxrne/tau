# tau server

The TCP server binary. Exposes a `libtau` executor over a line-oriented TCP protocol.

## Wire protocol

One statement per line in, one response line out. Statements are the same queries accepted by the library executor. Responses follow a small set of patterns:

| Response | Meaning |
|----------|---------|
| `OK` | DDL or write succeeded |
| `OK BYE` | Server acknowledged `QUIT` / `EXIT` |
| `VAL <codec>` | Point lookup returned a value |
| `VAL NIL` | Point lookup: no tau covers that timestamp |
| `RANGE <n>; <s>:<e>:<v> ...` | Range scan returned `n` segments |
| `NAMES <n>; name ...` | Name list from `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS` |
| `GRANTS <n>; <user> <db>:<perms> ...; ...` | Output of `SHOW GRANTS` |
| `ERR <message>` | Parse failure, executor error, or permission denial |

Values are encoded with a one-character type tag: `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

### Authentication handshake and multi-user authorisation

When `--auth` is set, the first message from every client must be `AUTH <username> <password>`. Any other message before authentication is answered with `ERR authentication failed` and the connection is closed. After a successful `AUTH`, the session is bound to that user and every subsequent statement is dispatched through `Executor::exec_as` so the user's CRUDA grants are enforced.

```
> AUTH admin s3cr3t
< OK
> CREATE DATABASE main
< OK
> CREATE USER alice PASSWORD "p4ss"
< OK
> GRANT R ON main TO alice
< OK
```

Two user-store modes:

- **In-memory single user**: `--auth --username admin --password s3cr3t`. Bootstraps one global-admin user; no persistence. Convenient for ephemeral dev.
- **Persistent multi-user**: `--auth --users-file /var/lib/tau/users`. On first run with `--username`/`--password` the file is seeded with that user as global admin; afterwards the file is the source of truth and every `CREATE USER` / `DROP USER` / `GRANT` / `REVOKE` is atomically rewritten to it.

Permission model (CRUDA bitmap, per database, plus wildcard `"*"`):

| bit | grants                                       |
|-----|----------------------------------------------|
| `C` | `CREATE LENS`, `DERIVE LENS`                 |
| `R` | `AT`, `RANGE`, `REDUCE`, `SHOW LENSES`       |
| `U` | `APPEND LENS`, `COPY LENS`                   |
| `D` | `DROP LENS` (also `DROP DATABASE` with `A`)  |
| `A` | admin - manage users, grant/revoke, create DBs |

Effective permissions for user U on database D = `grants[D] | grants["*"]`. A user with `A` on `"*"` is a **global admin** - required for `CREATE DATABASE`, `CREATE USER`, `DROP USER`, `SHOW USERS`, and `SHOW GRANTS` of other users. Promotion is just `GRANT A ON * TO <user>`.

`SHOW DATABASES` is automatically filtered for non-admin callers to only databases they hold any grant on.

## Metrics endpoint

When started with `--metrics-port <PORT>`, the server spawns a lightweight HTTP listener on that port and serves Prometheus-format metrics at `GET /metrics` and a liveness body at `GET /healthz`. The endpoint is on a separate port from the query port so it can be firewalled independently. Every request is logged at `debug` (method, path, status, bytes, elapsed_us); raise `--log-level trace` to log raw request lines.

```bash
cargo run --release -- --metrics-port 9090
# then in another terminal:
curl http://127.0.0.1:9090/metrics
curl http://127.0.0.1:9090/healthz
```

The exposition includes per-statement-type counters, latency histograms in microseconds (`tau_statement_duration_microseconds_bucket{type=...,le=...}`), the per-type cumulative nanosecond counters used by the dst harness, security counters (auth attempts/failures, rejected connections, errors), and process gauges (`tau_process_resident_bytes`, `tau_process_virtual_bytes`, `tau_process_open_fds`, `tau_process_threads`, `tau_process_uptime_seconds`). All counters use `Relaxed` atomics; they are best-effort observability data, not synchronisation barriers.

## Concurrency model

Each incoming connection gets its own OS thread. All threads share one `Arc<RwLock<Executor>>`. Read-only statements (`AT`, `RANGE`, `REDUCE`, `SHOW DATABASES`, `SHOW LENSES`) take the read lock and run concurrently. Mutating statements (`CREATE`, `APPEND`, `COPY`, `DERIVE`, `DROP`) take the write lock exclusively.

When a connection is inside a `START TRANSACTION … COMMIT` block, individual mutation statements are buffered in memory under the write lock as usual but their changes are not committed to storage until the `COMMIT` statement is processed. At commit time the write lock is held for the entire batch so concurrent readers see either none or all of the transaction's writes. `ROLLBACK` acquires the write lock only to discard the buffer.

This is a simple and correct model. The tradeoff is that a slow write (e.g. a WAL fsync with a slow disk) blocks all concurrent reads. For write-heavy workloads this can become a bottleneck. A per-database lock rather than a single global lock would improve write-read concurrency, but adds complexity.

### Connection capacity

`--max-connections N` (default 1024) caps the number of concurrent client threads. New connections beyond the cap are accepted, immediately answered with `ERR server at connection limit`, and counted in `tau_rejected_connections_total`. The accept loop tracks in-flight work with a single `AtomicUsize`, so the cap is a cheap fence rather than a bounded executor pool.

`--idle-timeout-secs SECS` (default 300, `0` disables) installs a per-socket read and write timeout. A connection that goes the full window without I/O is closed by the OS, freeing the thread.

## TLS

Pass `--tls` to enable. With no cert/key paths provided, an ephemeral self-signed certificate is generated at startup - convenient for development but not for production (clients cannot verify the server identity). For production, provide `--tls-cert` and `--tls-key` pointing to PEM-encoded files.

TLS is handled by `rustls`. The server does not perform client certificate authentication.

## Encryption at rest

Set `TAU_ENCRYPTION_KEY` to a 64-character hex string (32 bytes) in the environment. WAL entries are then encrypted per-entry with AES-256-GCM. Without the key, the WAL can be read but the encrypted entries will be skipped during replay. An unencrypted WAL remains readable when no key is set, so the feature is backward-compatible.

## Graceful shutdown

There is no userland signal handler yet; `SIGTERM` / `SIGINT` will kill the process immediately. WAL writes are fsynced before each statement's response is returned, so durability is preserved at the per-statement boundary; only in-flight statements may be lost. A drain/quiesce path is on the roadmap.

## Design decisions

### Thread-per-connection instead of async

The server uses `std::thread` rather than Tokio or async-std. For a database server where each connection is long-lived and query processing is CPU-bound (compaction, expression evaluation) rather than I/O-bound, synchronous threads are simpler to reason about and debug. Async would complicate the `RwLock` usage significantly without a clear throughput gain for the expected connection counts.

If Tau ever needs to support thousands of concurrent connections (unlikely for an embedded TSDB), the threading model should be revisited.

### Single executor for all databases

All named databases live inside one `Executor` protected by one lock. This keeps startup simple - there is no per-database file or configuration - but it means the lock is contended across all databases. A future multi-tenant mode might want per-database executors and per-database locks.
