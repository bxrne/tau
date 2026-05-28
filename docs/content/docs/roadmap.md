+++
title = "Roadmap"
date = 2026-05-28
template = "page.html"
+++

Tau is in active development. The current release is **v0.1.0**. Future versions are marked **(soft)** — they describe intent and scope, not a committed schedule.

---

## v0.1.0 — the engine is complete

The core engine and server are feature-complete and shipping. The data model, query language, storage backends, and simulation testing infrastructure are all in this release.

**Engine**
- Half-open interval model with monoid concatenation semantics
- Newest-layer-wins resolution by monotonic layer ID — deterministic, no configuration
- O(log n) point lookup per layer; sweep-line normalisation compaction
- Derived lenses with lazy closure composition and cycle detection at `DERIVE` time
- Rolling window aggregations as first-class expression nodes
- `Arc`-backed immutable layers — clones are pointer bumps

**Storage**
- In-memory and binary disk backends
- AES-256-GCM encryption at rest; per-entry CRC32 integrity
- Write-ahead log with per-statement fsync and full WAL replay on startup
- Schema DDL (`CREATE LENS` / `DERIVE LENS`) persisted and replayed
- WAL checkpoint after compaction

**Query language (TauQL)**
- `CREATE / DROP / USE DATABASE`; `SHOW DATABASES / LENSES`
- `CREATE / DROP LENS` with static types
- `APPEND LENS`; `COPY LENS FROM` for server-side CSV ingest
- `DERIVE LENS AS <expr>`: lazy computed lenses with composable closures
- `AT`, `RANGE [WHERE <expr>]`, `REDUCE USING (min|max|avg|sum|count)`
- Full expression grammar: arithmetic, comparison, logical, unary, rolling aggregations

**Server**
- Line-oriented TCP protocol with shared/exclusive locking
- TLS (PEM cert/key or ephemeral self-signed)
- Argon2id authentication; per-database CRUDA grants; wildcard grants
- `CREATE / DROP USER`, `GRANT / REVOKE`, `SHOW USERS / GRANTS`
- Connection cap with graceful rejection; per-connection idle timeout
- `GET /healthz` liveness probe; Prometheus metrics via `--metrics-port`

**Verification**
- Property-based tests (Hegel/Hypothesis): interval containment, layer lookup, value roundtrip, compaction query-equivalence, permission composition — each checked against hundreds of randomised inputs
- Deterministic simulation tester (`dst`): every transport × auth × WAL combination, driven against a reference oracle, with fault injection and reproducible seeds

---

## v0.2.0 (soft) — performance and operability

The engine is correct. v0.2.0 makes it fast enough to benchmark honestly, operable enough to run in production without documentation gaps, and expressive enough to cover real ingest and audit patterns.

**Benchmarks**
- Published `cargo bench` suite using Criterion: `AT`, `RANGE`, `REDUCE`, and `APPEND` at varying layer counts and dataset sizes
- Reproducible comparison against InfluxDB 2.x and QuestDB on standard ingest and query workloads, with methodology documented and results checked into the repo
- Flamegraph-guided profiling; all regressions caught by the bench suite in CI

**Query performance**
- Multi-layer merge iterator: single-pass query across N layers instead of N sequential passes — the primary hot path as layer count grows before compaction
- Write throughput profiling and targeted optimisation of the WAL path

**Transactions and batch ingest**
- `BEGIN` / `COMMIT` / `ROLLBACK`: atomic multi-statement transaction — all `APPEND` statements inside a transaction are committed as a single layer, eliminating per-statement layer churn and reducing compaction pressure; supports touching multiple lenses atomically
- `BATCH APPEND LENS <name> { ... }`: single-statement bulk ingest for one lens — a list of intervals inside a block, committed as one layer without round-trip overhead; optimised for high-volume ingest paths

**Layer introspection and audit**
- `HISTORY LENS <name> [start end]`: list all layers covering a time range, with their IDs, write timestamps, and interval coverage — answers "how many corrections have been applied here and when?"
- `AT LENS <name> <t> AS OF <timestamp>`: point query against the state of the data as it existed at a given wall-clock time, using write timestamps recorded in the WAL; the user-facing audit API
- `AT LENS <name> <t> LAYER <n>`: low-level audit query against a specific layer ID — used for debugging and by the DST

**Backup and restore**
- `BACKUP DATABASE <name> TO <path>`: consistent snapshot — quiesces the WAL, copies the store file and a WAL checkpoint, resumes; atomic from the caller's perspective
- `RESTORE DATABASE <name> FROM <path>`: replays a backup into a running server with WAL consistency checks
- Tested against real failure scenarios: partial backup, interrupted restore, corrupt snapshot

**Configuration**
- TOML configuration file replaces long flag lists; all current flags map to config keys
- Config file is the canonical source of truth; flags override for one-off runs
- Required groundwork for v0.3.0 cluster configuration

**Client**
- Python client (`pip install tau-py`): connection management, authentication, `AT`, `RANGE`, `REDUCE`, `APPEND`, `BATCH APPEND`, and transaction support
- Thin enough to drive the benchmark suite from a notebook

---

## v0.3.0 (soft) — distributed

v0.3.0 makes Tau a multi-node system. The layer model is already well-suited to replication: layers are append-only, have monotonic IDs, and conflict resolution is a deterministic rule. The main addition is a consensus layer that assigns globally ordered layer IDs and replicates the WAL across nodes.

**Replication model**
- Raft consensus via `openraft`: the WAL maps directly to a Raft replicated log — each WAL entry becomes a Raft log entry, so the distributed storage layer is largely already written
- The leader node assigns globally monotonic layer IDs; the algebraic properties of the layer model are preserved exactly across the cluster
- Read replicas serve `AT`, `RANGE`, and `REDUCE` with bounded replication lag; writes always route to the leader
- `CONSISTENCY STRONG` query hint routes a read to the leader for linearisable results

**Fault tolerance**
- Leader election and automatic failover via Raft
- Cluster recovers from minority node loss without data loss and without manual intervention
- The DST is extended to multi-node: simulates leader failures, network partitions, and follower lag, cross-checking every query result against the reference oracle

**Cluster management (TauQL)**
- `CLUSTER STATUS`: node list, leader, replication lag per follower
- `ADD NODE <addr>`, `REMOVE NODE <id>`: online membership changes via Raft joint consensus
- `tau` binary gets `--cluster`, `--peers <addr,...>`, and `--node-id` flags; cluster config lives in the TOML config file

---

## Backlog

Items with no assigned version — considered, not yet scheduled.

- Named timestamp aliases: ISO-8601 or human-readable offsets in `AT` and `RANGE`
- `systemd` unit file for local deployments
- `man` page for the server binary
- Online schema evolution: rename lens, change type with migration
- Go client for integration-test ergonomics
- Grafana data source plugin
- Prometheus remote write adapter: ingest Prometheus metrics directly into Tau lenses
- Leaderless replication via hybrid logical clocks: the algebraic approach to multi-master — post-v0.3.0 research direction
