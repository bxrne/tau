+++
title = "Why Tau: a time-series database that never forgets"
date = 2026-05-28
template = "page.html"
[taxonomies]
tags = ["release", "design"]
+++

Time series data has a problem that most databases aren't built to handle well: **the past keeps changing**.

A sensor reading you stored at noon might need to be corrected at 2 PM. A financial price gets restated. A billing entry gets amended. An audit log gets a footnote. In a traditional database, you update the row. The old value disappears. If you were careful, you kept a copy in a history table. If you weren't, the original fact is gone forever.

Tau is built on a different assumption: **corrections are normal, and the history of corrections is valuable**.

---

## The immutable layer model

Every value in Tau is a *temporal interval*: a value `V` that was true from time `A` to time `B`. When that fact changes, you don't update it. You append a new layer on top.

```
APPEND LENS temperature 0 3600 18.5  ← original reading
...
APPEND LENS temperature 1800 3600 19.2  ← correction arrives
```

At query time, the newest layer covering a timestamp wins. But the old layer is still there. You can see the original reading, the correction, and the correction to the correction, all from the same data. You didn't have to design that history table in advance. It's free.

This isn't just philosophically tidy. It has practical consequences:

**Concurrent writes never conflict.** Two clients appending to the same lens at the same time will both succeed. Resolution is lazy: newest layer wins at query time. No locking, no conflict detection, no retry loops.

**Out-of-order data is fine.** Sensor readings often arrive late. In Tau, a late reading is just a layer with an earlier timestamp. The query resolution handles it automatically.

**You can audit corrections without extra work.** Want to know what the temperature reading looked like before the 2 PM correction? Query from a snapshot, or scan the underlying layers. It's all there.

---

## Derived lenses: temporal computation as data

The other idea in Tau is that *computation over time-series data should be a first-class database concept*, not something you build outside the database.

```
DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0
DERIVE LENS hot AS temperature > avg(temperature, -1800, 0)
DERIVE LENS req_rate AS requests / 60
```

These aren't views in the SQL sense; they're lazy closures compiled at `DERIVE` time. Every query evaluates the expression on demand. Nothing is materialised. The derived lens stays up to date automatically because it re-evaluates every time.

The `avg(temperature, -1800, 0)` expression evaluates the time-weighted average of `temperature` over the 30 minutes ending at the query point. This is a rolling window aggregation expressed as a first-class database object. You can chain derived lenses: `DERIVE smoothed_hot AS smoothed_temp > avg(smoothed_temp, -300, 0)` composes two derived lenses.

The result is that anomaly detection, unit conversion, smoothing, and threshold logic can all live inside the database, where they're always consistent with the underlying data.

---

## What Tau is and isn't

Tau is not a replacement for PostgreSQL or ClickHouse. It doesn't do joins. It has no SQL. There are no indexes to tune, no query planner to fight, no schema migrations to coordinate.

Tau is a purpose-built temporal store for a specific class of problem: data where you need to record how things change over time, where corrections are normal, where the history matters, and where you want to express time-based computations close to the data.

If your data fits that shape (sensor streams, financial time series, audit logs, monitoring data), Tau gives you a data model that matches your domain directly, without the impedance mismatch of forcing temporal semantics onto a row-mutation model.

---

## Correctness as a first principle

Tau is young. The design prioritises correctness over features rather than claiming production-hardened battle-testing.

The deterministic simulation tester (`dst`) exercises the engine across hundreds of millions of operations, simulating centuries of temporal data, injecting faults (connection drops, WAL truncation), and cross-checking every result against a simple reference oracle. The seed that found a bug six months ago can be re-run against today's binary to confirm the fix. No flaky tests. No Heisenbugs.

Property-based testing (via Hegel, backed by Hypothesis) covers the core invariants: layer at-queries match linear scans, value encoding roundtrips, permission checks fire on the right conditions. These run automatically in CI.

The WAL design is conservative: every write is fsynced before the response is returned. Crash recovery replays the WAL exactly. The only risk is a duplicate replay, which the idempotent append semantics handle gracefully.

---

## Why now

Time-series data is ubiquitous and growing. IoT fleets, monitoring stacks, financial feeds and audit systems all produce streams of facts that change over time. Most are jammed into relational databases or columnar stores that weren't designed for temporal semantics, with history tables bolted on as an afterthought.

The tools that exist (InfluxDB, TimescaleDB, QuestDB, etc.) are powerful and mature, but largely optimised for append-only sensor data at high throughput. Correction (the restatement of a past fact) is either unsupported, awkward, or requires building your own layer management.

Tau's immutable layer model makes correction a primitive. That's the gap it fills.

---

## Try it

```bash
# Local
git clone https://github.com/bxrne/tau
cargo run --release

# Docker
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Connect:

```bash
cargo run --release --bin ctl
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE demo
τ: CREATE LENS temperature float
τ: APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
τ: AT LENS temperature 1800
VAL f18.5
```

→ [Documentation](https://tau.bxrne.com/docs/) | [GitHub](https://github.com/bxrne/tau)
