# Tau Roadmap to 1.0

This document tracks what has been built and what remains before Tau can be considered production-ready. Granularity is intentionally at the feature/correctness level rather than individual commits - the aim is a map that communicates intent, not a sprint board.

## Foundation

- [x] Core data model - `Tau<V>` (half-open interval), `Layer<V>` (sorted, non-overlapping batch), `Lens<V>` (base or derived)
- [x] Immutable correction semantics - newest layer wins, old data never mutated
- [x] O(log n) point lookup via binary search within a layer
- [x] Auto-compaction - threshold-based merge of multiple layers into one canonical layer
- [x] Derived lenses - lazy expression evaluation at query time, no caching
- [x] `Arc`-backed cheap layer clones - reads share allocations with no copying

## Storage

- [x] In-memory backend (`InMemory`) - HashMap-based, suitable for tests and ephemeral workloads
- [x] Binary disk backend (`Disk`) - length-prefixed entries, 16-byte header with magic + CRC32, per-entry checksums
- [x] Disk encryption at rest - AES-256-GCM with random 12-byte nonce per flush (`TAUE` magic)
- [x] Write-ahead log (`Wal`) - append-only flat file, fsync before returning, per-line CRC32 checksums
- [x] WAL encryption - per-entry AES-256-GCM, base64-encoded with `E:` prefix, plaintext entries remain readable without a key
- [x] WAL replay on startup - reconstruct in-memory state from durable log
- [x] **WAL log rotation / truncation** - after auto-compaction, `Database::checkpoint` rewrites the WAL to contain only the live (post-compaction) layers; WAL growth is bounded
- [x] **Schema persistence in WAL** - `CREATE LENS` and `DERIVE LENS` are written as `S:` lines in the WAL; replay restores both data and schema so lens declarations survive a restart
- [x] **Disk backend append mode** - unencrypted `Disk` now holds an open append-mode file handle; `Store::append` writes each entry in O(entry) time; a full rewrite only occurs when compaction fires
- [x] **WAL write failure handling** - `Database::append` returns `io::Result<()>`; a WAL fsync failure leaves the in-memory store unchanged; `ExecError::Io` surfaces the error to the client over TCP

## Query Language

- [x] Parser - nom-based combinator, case-insensitive keywords, full operator-precedence grammar
- [x] `CREATE / DROP / USE DATABASE`
- [x] `CREATE / DROP LENS` with static type declarations (`int`, `float`, `str`, `bool`, `bytes`)
- [x] **Multi-tau `APPEND`** - `APPEND LENS name s0 e0 v0 [, s1 e1 v1 …]` writes one or more taus as a single layer; bulk form reduces per-write overhead and is atomic (all-or-nothing type validation)
- [x] **`COPY LENS <name> FROM "<path>"`** - ingest taus from a CSV file (`start,end,value` per line); blank lines and `#` comments skipped; entire file batched into one layer
- [x] `DERIVE LENS AS <expr>` - define a computed lens
- [x] **`SHOW DATABASES` / `SHOW LENSES`** - introspection queries; `NAMES <n>; name …` wire response; read-only, uses shared lock
- [x] `AT LENS <t>` - point lookup
- [x] `RANGE LENS <start> <end> [WHERE <expr>]` - materialise segments with optional filter
- [x] `REDUCE LENS <start> <end> USING <func>` - scalar aggregate over a range (`min`, `max`, `avg`, `sum`, `count`)
- [x] Rolling / windowed aggregation in expressions - `avg(lens, rel_start, rel_end)`
- [x] Arithmetic, comparison, and logical operators in expressions
- [ ] **Named timestamp aliases** - allow symbolic names (e.g. Unix epoch offsets, ISO-8601 parsing) rather than raw integers only

## Executor

- [x] Per-database lens registry with type enforcement
- [x] Null is type-compatible with any declared type
- [x] Derived lens definitions stored as AST nodes - re-evaluated live on every query
- [x] `exec_read` / `exec` split for lock routing
- [x] **Schema persistence** - `CREATE LENS` and `DERIVE LENS` are written as `S:` WAL entries; replay restores both data and schema declarations on restart
- [x] **Cycle detection in derived lenses** - `DERIVE LENS` performs a DFS through the existing derived graph before inserting; returns `CycleDetected` error for direct or transitive cycles
- [x] **`RANGE` on aggregation-backed derived lenses** - boundary collection for `Agg` expressions corrected; enter/exit projection ranges computed from `min`/`max` of relative offsets, covering all sign combinations

## Server

- [x] Line-oriented TCP protocol - one statement per line in, one response per line out
- [x] Concurrent read routing - `AT`, `RANGE`, `REDUCE` take a shared read lock; mutations take an exclusive write lock
- [x] TLS encryption in transit - `--tls`, optional PEM cert/key, ephemeral self-signed cert for development
- [x] Argon2id password authentication - `--auth --username --password`; hash computed at startup, plaintext not retained
- [x] **Multi-user authentication with per-database CRUDA grants** - `--users-file PATH` enables a file-backed `UserStore`; `Executor::exec_as` enforces a 5-bit Create/Read/Update/Delete/Admin bitmap per database (with `"*"` as a wildcard); `CREATE USER`, `DROP USER`, `GRANT`, `REVOKE`, `SHOW USERS`, `SHOW GRANTS` are first-class statements gated on global admin (`A` on `*`); promotion is just `GRANT A ON * TO <user>`. Bootstrap admin via `--username/--password` on the first run; thereafter the file is source of truth (atomic rewrite on every mutation)
- [x] Structured logging via `tracing` - auth success/fail, per-query elapsed-µs + ok/err status (debug), accepted/disconnected peers (debug/info), TLS/no-TLS startup
- [x] **Connection limits** - `--max-connections N` (default 1024) caps concurrent client threads. Excess connections are rejected at accept with `ERR server at connection limit` and counted in `tau_rejected_connections_total`
- [x] **Per-connection idle timeout** - `--idle-timeout-secs SECS` (default 300, 0 disables). Both the read and write halves get the timeout via `set_read_timeout` / `set_write_timeout`
- [x] **Health / readiness endpoint** - `GET /healthz` on the metrics port returns 200 with a short body; suitable for Kubernetes/Nomad-style probes
- [x] **Metrics** - Prometheus-style counters, histograms (per-type latency in microseconds, with the standard OpenMetrics buckets) and gauges (RSS, VSZ, open FDs, threads, uptime). Reachable on `--metrics-port`; trace-logged per request
- [ ] **Graceful shutdown** - `SIGTERM` / `SIGINT` handling; in-flight connections should drain before exit

## Operational

- [ ] **Config file support** - CLI flags only; a TOML/YAML config file would allow persistent server configuration without shell wrappers
- [ ] **Backup and restore tooling** - snapshot the WAL + Disk files safely while the server is live
- [x] **`tauctl` CLI tool** - interactive REPL (`src/bin/tauctl/`) speaking the wire protocol with terminal colors, named connection pool, plain-TCP **and TLS** transport (no-verify verifier for self-signed dev certs), inline `AUTH` at `connect` time or via a standalone `auth` command, status footer with elapsed time + ok/err, fall-through dispatcher so any non-built-in input is forwarded as a tauql statement, `rustyline`-backed input (arrow keys, history persisted to `$HOME/.tau_history`, bracketed paste, Ctrl-A/E/W/etc.), and a client-side `load <lens> <local-path>` command that ships local CSVs to the active connection as batched `APPEND` statements
- [x] **Docker image and `docker-compose` stack** - scratch-based musl static image (`ghcr.io/bxrne/tau`); compose file under `container/` brings up tau + Prometheus + Grafana with provisioned dashboards and alert rules; full GHCR pull / TLS / auth flows documented in `container/README.md`
- [ ] **`systemd` unit file** - for bare-metal deployments
- [x] **Benchmark harness** - `src/bin/bench/main.rs` spawns a real `tau` server per cell and measures throughput via the Prometheus `/metrics` endpoint; covers plain/TLS transport, no-auth/password auth, and WAL on/off; emits one CSV row per cell and supports `--scratch DIR` for tmpfs-vs-real-disk runs. Drove the optimizations: sweep-line compaction, opt-in non-fsync mode (`Wal::set_fsync_each`, `Disk::set_fsync_each`, `Disk::set_rewrite_on_compact`, `Database::set_auto_checkpoint`), batched record writes, `HashMap` instead of `BTreeMap` for the Disk lens table

## Correctness and Safety

- [ ] **Fuzz testing** - the parser and WAL deserialiser are the two surfaces most likely to misbehave on adversarial input; `cargo fuzz` targets should cover both
- [ ] **End-to-end integration tests** - TCP-level tests that spin up a real server process and drive it with raw socket writes, verifying auth, TLS, and WAL replay end-to-end
- [ ] **Property-based tests for compaction** - verify that `compact_layers` produces identical query results to the uncompacted stack for any sequence of layers

## Documentation

- [ ] **Protocol specification** - a standalone document covering the full wire format, all response codes, error messages, and authentication handshake; necessary before external clients can be written
- [ ] **Operational guide** - how to size the WAL, tune the compaction threshold, rotate encryption keys
- [ ] **`man` page** for the server binary

## Client Ecosystem

Tau speaks plain TCP so any language with a socket library can drive it directly - no formal SDK is strictly required. That said:

- [ ] **Reference client in Rust** - a typed async client crate that handles connection management, auth, and response parsing; also serves as a protocol correctness reference
- [ ] **Thin clients for Python and Go** - enough to make integration tests from those ecosystems natural; thin wrappers around socket I/O, not full-featured ORMs

## Release Gate (1.0)

The following must all be true before tagging 1.0:

- All items above this line marked done (or explicitly deferred with a documented reason)
- The `bench` binary reports stable numbers on the reference hardware
- End-to-end TCP tests pass in CI
- The protocol specification document exists
- Docker image builds and the `docker-compose` example starts cleanly
