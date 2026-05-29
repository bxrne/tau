+++
title = "Introducing Tau"
date = 2026-05-29
template = "page.html"
[taxonomies]
tags = ["release", "tau"]
categories = ["tau"]
+++

Most time series databases assume facts do not change. A sensor reading is what it was. A price tick is final. Once written, data is past.

The truth is messier. Sensors drift and need recalibration after the fact. Prices get restated. Risk numbers are revised. Audit records are amended. Healthcare measurements arrive late, out of order, sometimes from devices with the wrong clock. In every one of those settings the question "what did we believe at time t" matters as much as "what was true at time t".

Tau is a time series database built for that question.

## The model

Every value in Tau has a time range. Not a timestamp. A range.

```
Tau { start: i64, end: i64, value: V }   // value V was true over [start, end)
```

Intervals are half open. They include `start` and exclude `end`. Adjacent intervals tile cleanly. There are no gaps, no overlaps and no ambiguity about which interval owns a boundary timestamp.

A `Layer` is an immutable batch of intervals appended together. A `Lens` is a named stack of layers, optionally derived from other lenses.

When data arrives, it becomes a new layer. When data is corrected, the correction also becomes a new layer. The old layer is never touched. At query time the newest layer wins at any point where two layers cover the same timestamp.

This is the bitemporal pattern. The transaction time of a write is decoupled from the valid time of the fact. Both are recorded. Either can be queried.

## Why a new database

Three reasons the existing options do not fit this workload.

First, the dominant time series databases assume append only data. InfluxDB, Prometheus and Graphite all treat writes as additions to a series. Corrections require deleting and rewriting, which loses the history of the correction itself. The audit trail goes with it.

Second, the relational stores that handle bitemporal data well (the Crux and XTDB lineage, certain Postgres extensions) carry the full weight of a general purpose query planner. For time series queries that is overhead which does not pay for itself.

Third, the systems that handle out of order arrivals through reprocessing pipelines (Flink, Kafka stream tables, Iceberg) push the problem upstream. The database itself does not preserve the correction history. The pipeline does, and only if it was configured to.

Tau aims for a smaller, sharper target. A storage engine where corrections are a primitive operation. A query language where the time aware operators are first class. A verification strategy where correctness across all of these states is checked on every build.

## A correction in practice

```
CREATE DATABASE sensors
CREATE LENS temperature float

APPEND LENS temperature 0 3600 18.5
AT LENS temperature 1800        -> VAL f18.5

APPEND LENS temperature 0 3600 20.0     // correction over the same range
AT LENS temperature 1800        -> VAL f20

REDUCE LENS temperature 0 7200 USING avg  -> reflects the correction
```

The second `APPEND` does not overwrite. It adds a new layer with a higher ID over the same range. Old data stays on disk. The point query returns the corrected value. The aggregate reflects the correction.

If a later layer needs to be revealed (for audit, for time travel, for a "as we believed it" reconstruction) the data is there. None of this is configuration. It is the model.

## Compaction without information loss

Layers accumulate. A naive query walks layers newest first until it finds a covering interval. With many layers that walk is linear in the layer count.

Compaction collapses N layers into one canonical layer using a sweep line algorithm.

```
1. Collect all interval start and end events across every layer in the lens.
2. Sort by timestamp. Ends before starts at ties.
3. Walk events. Track active layers in a max heap keyed by layer ID.
4. The layer with the highest active ID wins. Emit a segment whenever
   the winning value changes.
```

The result is one layer. Every `AT`, `RANGE` and `REDUCE` query returns the same answer it did before compaction. The equivalence is not a claim. It is a property checked by the property based test suite on every build.

Compaction is a normalisation. It reduces query cost. It does not change what queries return.

## Derived lenses

`DERIVE LENS` builds a new lens from an expression over existing lenses.

```
DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0
DERIVE LENS cpu_smooth AS avg(cpu, -600, 0)
DERIVE LENS cpu_hot AS cpu > avg(cpu, -1800, 0)
```

The expression is compiled into a lazy closure at definition time. Nothing is materialised. Every query re evaluates the expression against the current data.

Closures compose. A lens derived from other derived lenses chains their closures. Rolling window aggregations like `avg(cpu, -600, 0)` are first class expression nodes. They can appear in `DERIVE`, in a `WHERE` clause on a range scan, or as the basis for a threshold lens.

Cycle detection runs at definition time by walking the dependency graph. A lens that would cause a cycle is rejected before the schema is committed.

## Verification

A database that records corrections is only useful if the corrections preserve query meaning across compaction. Without that guarantee, compaction is a footgun. With it, compaction is a free operation that never changes a result.

Tau verifies this on every build through three layers.

**Unit tests** anchor known shape behaviour. Wire protocol responses, error message strings, parse failures, auth rejection sequences.

**Property based tests** (Hegel and Hypothesis) check invariants across randomised inputs. Layer containment, the compaction equivalence property, value codec roundtrips, permission composition. Each property runs hundreds of randomised cases. Failures shrink to the smallest reproducer.

**Deterministic simulation testing** drives the engine end to end against a reference oracle. The oracle is a `BTreeMap<start, (end, value)>` per lens with no layers, no compaction and no WAL. Just obviously correct temporal semantics. Every operation processed by the engine is also processed by the oracle. Every read is checked against both. Any divergence stops the run and prints the seed.

The DST drives every transport, auth and WAL configuration cell. It injects faults. It is reproducible from a single seed. It is inspired by the simulation testing tradition at FoundationDB and TigerBeetle. A dedicated [page](/docs/dst/) covers it in depth.

## Use cases

The shape of Tau favours workloads where the history of corrections is itself interesting.

**Sensor telemetry.** Devices recalibrate. Devices go offline and replay. Devices have clock skew. Tau stores every layer, including the late ones, with no work on the ingest side.

**Financial restatements.** Prices, marks, NAVs and risk numbers get revised. Audit, compliance and reconciliation all need to see what was believed at the time of the original write. Tau records both.

**Audit trails.** Bitemporal data is the standard model for compliance reporting. What did the system know about this customer at the time of the decision. Tau stores it directly.

**Embedded analytics.** `libtau` runs as a Rust library with no server process. Same engine as the server, same semantics, no network overhead.

## Getting started

```sh
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Or from source:

```sh
git clone https://github.com/bxrne/tau && cd tau
cargo run --release
```

Connect with the REPL.

```sh
cargo run --release --bin ctl
```

```
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE sensors
τ: CREATE LENS temperature float
τ: APPEND LENS temperature 0 3600 18.5
τ: AT LENS temperature 1800
VAL f18.5
τ: APPEND LENS temperature 0 3600 20.0
τ: AT LENS temperature 1800
VAL f20
```

The second `APPEND` is a correction. The original layer is still on disk. The new query returns the corrected value. The aggregate reflects the correction.

## Where it is going

v0.1 ships the engine. Half open intervals, layered storage, compaction, TauQL, WAL durability, TLS, authentication with CRUDA grants, encryption at rest and the deterministic simulation tester.

v0.2 brings published benchmarks, batch ingest, layer introspection (`HISTORY LENS`) and the `AT LENS x t AS OF wall_clock` time travel query that gives the bitemporal story its strongest expression. A Python client lands in the same release.

v0.3 brings Raft based replication via openraft. The WAL maps directly to a Raft replicated log, so the distributed storage layer is largely already written. The leader assigns globally monotonic layer IDs and the algebraic properties of the layer model survive across the cluster.

The full roadmap is [here](/docs/roadmap/).

## Read more

- [Overview](/docs/overview/). The data model in depth.
- [How it works](/docs/how-it-works/). Storage, WAL, compaction, concurrency.
- [DST](/docs/dst/). The simulation tester.
- [TauQL reference](/docs/tauql/). Every statement and operator.
- [Examples](/docs/examples/). Worked queries against real datasets.

Tau is open source under the Apache 2.0 license at [github.com/bxrne/tau](https://github.com/bxrne/tau).

Feedback, questions and pull requests are welcome.
