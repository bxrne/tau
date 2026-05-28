+++
title = "Roadmap"
date = 2026-05-28
template = "page.html"
+++

Tau is in active development. The current line is **v0.x**, where the engine matures and the operational story tightens. The destination is **v1.0**: a system you can trust under load, in production, without asking the maintainer how to do anything.

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
- Deterministic simulation tester (`dst`): every transport x auth x WAL combination, driven against a reference oracle, with fault injection and reproducible seeds

---

## v1.0 — the quality bar

v1.0 is not a feature list. It is a commitment that what is already here behaves correctly under adversarial conditions, and that a new operator can run it in production using only the written documentation.

**Correctness**
- The DST runs to completion without finding an invariant violation for any seed across the full operation space: compaction, WAL replay, derived lens composition, and authorisation all interact correctly under sustained simulated stress.
- Fuzz targets for the parser and WAL deserialiser have run for at least 24 hours without crashes or panics.
- Property-based tests cover `compact_layers` end-to-end: for any randomly generated layer sequence, every query result is identical before and after compaction.

**Operability**
- A new operator can deploy, configure, and monitor a Tau instance using only written documentation. No tribal knowledge required.
- A protocol specification describes the full wire format, every response code, and the authentication handshake.
- An operational guide covers WAL sizing, compaction tuning, and encryption key rotation.
- Graceful shutdown drains in-flight connections on `SIGTERM` and `SIGINT`.
- A TOML or YAML config file replaces long flag lists.

**Reliability**
- Online backup and restore tooling exists and is tested against real failure scenarios.
- WAL truncation mid-entry replays cleanly: no panic, no silent data loss.

**Client story**
- A typed async Rust client crate handles connection management, authentication, and response parsing.
- At least one thin client (Python or Go) exists for integration-test ergonomics.

---

## Backlog

- Named timestamp aliases: ISO-8601 or human-readable offsets
- `systemd` unit file for local deployments
- `man` page for the server binary
- Online schema evolution (rename lens, change type with migration)
