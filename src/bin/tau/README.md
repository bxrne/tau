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
| `RANGE <n>; <s>:<e>:<v> …` | Range scan returned `n` segments |
| `NAMES <n>; name …` | Name list from `SHOW DATABASES` / `SHOW LENSES` |
| `ERR <message>` | Parse failure or executor error |

Values are encoded with a one-character type tag: `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

### Authentication handshake and multi-user authorisation

When `--auth` is set, the first message from every client must be `AUTH <username> <password>`. Any other message before authentication is answered with `ERR authentication failed` and the connection is closed. After a successful `AUTH`, the session is bound to that user and every subsequent statement is dispatched through `Executor::exec_as` so the user's CRUDA grants are enforced.

```
→ AUTH admin s3cr3t
← OK
→ CREATE DATABASE main
← OK
→ CREATE USER alice PASSWORD "p4ss"
← OK
→ GRANT R ON main TO alice
← OK
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

Effective permissions for user *U* on database *D* = `grants[D] | grants["*"]`. A user with `A` on `"*"` is a **global admin** - required for `CREATE DATABASE`, `CREATE USER`, `DROP USER`, `SHOW USERS`, and `SHOW GRANTS` of other users. Promotion is just `GRANT A ON * TO <user>`.

`SHOW DATABASES` is automatically filtered for non-admin callers to only databases they hold any grant on.

## Concurrency model

Each incoming connection gets its own OS thread. All threads share one `Arc<RwLock<Executor>>`. Read-only statements (`AT`, `RANGE`, `REDUCE`, `SHOW DATABASES`, `SHOW LENSES`) take the read lock and run concurrently. Mutating statements (`CREATE`, `APPEND`, `COPY`, `DERIVE`, `DROP`) take the write lock exclusively.

This is a simple and correct model. The tradeoff is that a slow write (e.g. a WAL fsync with a slow disk) blocks all concurrent reads. For write-heavy workloads this can become a bottleneck. A per-database lock rather than a single global lock would improve write-read concurrency, but adds complexity.

**TODO:** There is no connection limit. A client that opens many connections simultaneously will exhaust the OS thread limit. A semaphore or bounded thread pool should gate connection acceptance before 1.0.

**TODO:** There is no per-connection idle timeout. Connections that stop sending data hold their thread indefinitely.

## TLS

Pass `--tls` to enable. With no cert/key paths provided, an ephemeral self-signed certificate is generated at startup - convenient for development but not for production (clients cannot verify the server identity). For production, provide `--tls-cert` and `--tls-key` pointing to PEM-encoded files.

TLS is handled by `rustls`. The server does not perform client certificate authentication.

## Encryption at rest

Set `TAU_ENCRYPTION_KEY` to a 64-character hex string (32 bytes) in the environment. WAL entries are then encrypted per-entry with AES-256-GCM. Without the key, the WAL can be read but the encrypted entries will be skipped during replay. An unencrypted WAL remains readable when no key is set, so the feature is backward-compatible.

## Graceful shutdown

**TODO:** There is currently no signal handler. `SIGTERM` or `SIGINT` will kill the process immediately. Before 1.0, the server should finish processing in-flight requests, flush any buffered WAL state, and then exit cleanly.

## Design decisions

### Thread-per-connection instead of async

The server uses `std::thread` rather than Tokio or async-std. For a database server where each connection is long-lived and query processing is CPU-bound (compaction, expression evaluation) rather than I/O-bound, synchronous threads are simpler to reason about and debug. Async would complicate the `RwLock` usage significantly without a clear throughput gain for the expected connection counts.

If Tau ever needs to support thousands of concurrent connections (unlikely for an embedded TSDB), the threading model should be revisited.

### Single executor for all databases

All named databases live inside one `Executor` protected by one lock. This keeps startup simple - there is no per-database file or configuration - but it means the lock is contended across all databases. A future multi-tenant mode might want per-database executors and per-database locks.
