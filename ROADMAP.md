# Tau Roadmap to 1.0

This document tracks what has been built and what remains before Tau can be considered production-ready. Granularity is intentionally at the feature/correctness level rather than individual commits — the aim is a map that communicates intent, not a sprint board.

---

## Foundation

- [x] Core data model — `Tau<V>` (half-open interval), `Layer<V>` (sorted, non-overlapping batch), `Lens<V>` (base or derived)
- [x] Immutable correction semantics — newest layer wins, old data never mutated
- [x] O(log n) point lookup via binary search within a layer
- [x] Auto-compaction — threshold-based merge of multiple layers into one canonical layer
- [x] Derived lenses — lazy expression evaluation at query time, no caching
- [x] `Arc`-backed cheap layer clones — reads share allocations with no copying

---

## Storage

- [x] In-memory backend (`InMemory`) — HashMap-based, suitable for tests and ephemeral workloads
- [x] Binary disk backend (`Disk`) — length-prefixed entries, 16-byte header with magic + CRC32, per-entry checksums
- [x] Disk encryption at rest — AES-256-GCM with random 12-byte nonce per flush (`TAUE` magic)
- [x] Write-ahead log (`Wal`) — append-only flat file, fsync before returning, per-line CRC32 checksums
- [x] WAL encryption — per-entry AES-256-GCM, base64-encoded with `E:` prefix, plaintext entries remain readable without a key
- [x] WAL replay on startup — reconstruct in-memory state from durable log
- [ ] **WAL log rotation / truncation** — WAL grows unbounded; after a compaction checkpoint it should be safe to truncate entries that are fully covered by the merged layer
- [ ] **Schema persistence in WAL** — `CREATE LENS` statements are not written to the WAL, so declared lens types are lost on restart; the replay path only reconstructs data, not the schema
- [ ] **Disk backend append mode** — `Disk::flush` rewrites the whole file; a proper append path would make individual writes O(entry) rather than O(total data)
- [ ] **WAL write failure handling** — currently panics if the WAL fsync fails; should return an error to the caller and leave the in-memory state unchanged

---

## Query Language

- [x] Parser — nom-based combinator, case-insensitive keywords, full operator-precedence grammar
- [x] `CREATE / DROP / USE DATABASE`
- [x] `CREATE / DROP LENS` with static type declarations (`int`, `float`, `str`, `bool`, `bytes`)
- [x] `APPEND LENS` — write a single tau to a base lens
- [x] `DERIVE LENS AS <expr>` — define a computed lens
- [x] `AT LENS <t>` — point lookup
- [x] `RANGE LENS <start> <end> [WHERE <expr>]` — materialise segments with optional filter
- [x] `REDUCE LENS <start> <end> USING <func>` — scalar aggregate over a range (`min`, `max`, `avg`, `sum`, `count`)
- [x] Rolling / windowed aggregation in expressions — `avg(lens, rel_start, rel_end)`
- [x] Arithmetic, comparison, and logical operators in expressions
- [ ] **Multi-tau `APPEND`** — currently each `APPEND` writes exactly one tau; bulk append would reduce per-write overhead
- [ ] **`COPY` / import statement** — ingest from CSV or line-delimited payloads
- [ ] **`SHOW DATABASES` / `SHOW LENSES`** — introspection queries; currently you must track what you created
- [ ] **Named timestamp aliases** — allow symbolic names (e.g. Unix epoch offsets, ISO-8601 parsing) rather than raw integers only

---

## Executor

- [x] Per-database lens registry with type enforcement
- [x] Null is type-compatible with any declared type
- [x] Derived lens definitions stored as AST nodes — re-evaluated live on every query
- [x] `exec_read` / `exec` split for lock routing
- [ ] **Schema persistence** — `DbState::base_types` and `derived` maps are lost on restart; must be serialised alongside the WAL
- [ ] **Cycle detection in derived lenses** — a derived lens that references itself (directly or transitively) will stack-overflow at query time
- [ ] **`RANGE` on aggregation-backed derived lenses** — boundary collection for nested `Agg` expressions inside `RANGE` filters is approximated; edge cases exist

---

## Server

- [x] Line-oriented TCP protocol — one statement per line in, one response per line out
- [x] Concurrent read routing — `AT`, `RANGE`, `REDUCE` take a shared read lock; mutations take an exclusive write lock
- [x] TLS encryption in transit — `--tls`, optional PEM cert/key, ephemeral self-signed cert for development
- [x] Argon2id password authentication — `--auth --username --password`; hash computed at startup, plaintext not retained
- [x] Structured logging via `tracing`
- [ ] **Graceful shutdown** — `SIGTERM` / `SIGINT` handling; in-flight connections should drain before exit
- [ ] **Connection limits** — unbounded `thread::spawn` per incoming connection; a slow-loris or connection flood will exhaust the thread pool or file descriptors
- [ ] **Client timeout** — connections with no activity should be reaped
- [ ] **Multi-user authentication** — currently a single global username/password; real deployments need per-user credentials and revocation
- [ ] **Health / readiness endpoint** — a simple TCP ping or HTTP endpoint that infra can probe without speaking the full query protocol
- [ ] **Metrics** — Prometheus-style counters and gauges (queries/sec, write latency, compaction events, layer counts) reachable via a separate port or endpoint

---

## Operational

- [ ] **Config file support** — CLI flags only; a TOML/YAML config file would allow persistent server configuration without shell wrappers
- [ ] **Backup and restore tooling** — snapshot the WAL + Disk files safely while the server is live
- [ ] **`tauctl` CLI tool** — a thin client that speaks the wire protocol, useful for scripting and administration
- [ ] **Docker image and `docker-compose` example** — minimal production-ready container with a named volume for the WAL
- [ ] **`systemd` unit file** — for bare-metal deployments
- [ ] **Benchmark harness** — the `bench` binary is referenced in `CLAUDE.md` but not yet fully implemented; covers append-seq, append-rand, point-lookup, range-scan workloads with InMemory and WAL backends

---

## Correctness and Safety

- [ ] **Fuzz testing** — the parser and WAL deserialiser are the two surfaces most likely to misbehave on adversarial input; `cargo fuzz` targets should cover both
- [ ] **End-to-end integration tests** — TCP-level tests that spin up a real server process and drive it with raw socket writes, verifying auth, TLS, and WAL replay end-to-end
- [ ] **Property-based tests for compaction** — verify that `compact_layers` produces identical query results to the uncompacted stack for any sequence of layers

---

## Documentation

- [ ] **Protocol specification** — a standalone document covering the full wire format, all response codes, error messages, and authentication handshake; necessary before external clients can be written
- [ ] **Operational guide** — how to size the WAL, tune the compaction threshold, rotate encryption keys
- [ ] **`man` page** for the server binary

---

## Client Ecosystem

Tau speaks plain TCP so any language with a socket library can drive it directly — no formal SDK is strictly required. That said:

- [ ] **Reference client in Rust** — a typed async client crate that handles connection management, auth, and response parsing; also serves as a protocol correctness reference
- [ ] **Thin clients for Python and Go** — enough to make integration tests from those ecosystems natural; thin wrappers around socket I/O, not full-featured ORMs

---

## Release Gate (1.0)

The following must all be true before tagging 1.0:

- All items above this line marked done (or explicitly deferred with a documented reason)
- Schema persistence: a server restart recovers both data **and** lens declarations
- WAL rotation is implemented so disk usage is bounded
- The `bench` binary reports stable numbers on the reference hardware
- End-to-end TCP tests pass in CI
- The protocol specification document exists
- Docker image builds and the `docker-compose` example starts cleanly
