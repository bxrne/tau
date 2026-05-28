+++
title = "Roadmap"
date = 2026-05-28
template = "page.html"
+++

The goal is to reach **v1.0**: a system trusted enough for production time-series workloads, correct under adversity, observable, and documented well enough that a new operator can run it without asking for help.

Current work ships as **v0.1.0**. The v0.x line is where the engine matures; features are complete but the operational story, correctness guarantees, and client ecosystem are still being hardened.

---

## v0.1.0 (current)

The core engine and server are feature-complete.

**Engine**
- Immutable, layered temporal intervals; newest-layer-wins semantics
- O(log n) point lookup; sweep-line compaction
- Derived lenses with lazy evaluation and cycle detection
- `Arc`-backed cheap layer clones

**Storage**
- In-memory and binary disk backends
- AES-256-GCM encryption at rest; per-entry CRC32 integrity
- Write-ahead log with fsync durability and WAL replay on startup
- Schema DDL (`CREATE LENS` / `DERIVE LENS`) persisted and replayed
- WAL checkpoint after compaction

**Query language (TauQL)**
- `CREATE / DROP / USE DATABASE`; `SHOW DATABASES / LENSES`
- `CREATE / DROP LENS` with static types
- `APPEND LENS`; `COPY LENS FROM` for server-side CSV ingest
- `DERIVE LENS AS <expr>`: lazy computed lenses
- `AT`, `RANGE [WHERE <expr>]`, `REDUCE USING (min|max|avg|sum|count)`
- Rolling window aggregations in expressions
- Full expression grammar: arithmetic, comparison, logical, unary

**Server**
- Line-oriented TCP protocol with shared/exclusive locking
- TLS (PEM cert/key or ephemeral self-signed)
- Argon2id authentication; per-database CRUDA grants; wildcard grants
- `CREATE / DROP USER`, `GRANT / REVOKE`, `SHOW USERS / GRANTS`
- Connection limit and per-connection idle timeout
- `GET /healthz` liveness probe

**Observability**
- Prometheus metrics via `--metrics-port`
- Structured `tracing` logs; per-query elapsed time

**Tooling**
- `tauctl` REPL with TLS, auth, named connection pool, history, client-side CSV load
- Docker image and `docker-compose` stack with Prometheus and Grafana
- Deterministic simulation tester (`dst`) covering all transport, auth, and WAL combinations

---

## v1.0 criteria

v1.0 is not a feature list. It is a quality bar.

**Correctness**
- The DST runs without finding an invariant violation for any seed across the full operation space: compaction, WAL replay, derived lenses, and authorization all interact correctly under simulated stress.
- Fuzz targets exist for the parser and WAL deserialiser and have run for at least 24 hours without crashes.
- Property-based tests cover `compact_layers` end-to-end: any layer sequence produces identical query results before and after.

**Operability**
- A new operator can deploy, configure, and monitor a Tau instance using only written documentation; no tribal knowledge required.
- A protocol specification describes the full wire format, all response codes, and the authentication handshake.
- An operational guide covers WAL sizing, compaction tuning, and encryption key rotation.
- Graceful shutdown drains in-flight connections on `SIGTERM`/`SIGINT`.
- A TOML/YAML config file replaces long flag lists.

**Reliability**
- Online backup and restore tooling exists and is tested.
- WAL truncation mid-entry replays cleanly with no panic and no silent data loss.

**Client story**
- A typed async Rust client crate handles connection management, auth, and response parsing.
- At least one thin client (Python or Go) exists for integration-test ergonomics.

---

## Backlog

- Named timestamp aliases: ISO-8601 or human-readable offsets
- `systemd` unit file for local deployments
- `man` page for the server binary
- Online schema evolution (rename lens, change type with migration)
