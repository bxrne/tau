+++
title = "Tau"
sort_by = "date"
paginate_by = 10
template = "index.html"
page_template = "page.html"
+++

**A time-series database built on algebraically precise temporal intervals, verified by property-based tests and deterministic simulation.**

Time-series data is not static. Sensors drift. Prices get restated. Audit records get amended. Tau models this directly: values live in half-open intervals `[start, end)` that tile without gaps or overlap, corrections append as new layers, and the newest layer wins at query time. Compaction normalises any stack of layers into a single canonical form — every query result is preserved exactly, a property checked by randomised tests on every build.

```
CREATE DATABASE sensors
CREATE LENS temperature float
APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
AT LENS temperature 1800        -> VAL f18.5
RANGE LENS temperature 0 7200  -> RANGE 2; 0:3600:f18.5; 3600:7200:f21
REDUCE LENS temperature 0 7200 USING avg  -> VAL f19.75

APPEND LENS temperature 0 3600 20.0
AT LENS temperature 1800        -> VAL f20
RANGE LENS temperature 0 7200  -> RANGE 2; 0:3600:f20; 3600:7200:f21
REDUCE LENS temperature 0 7200 USING avg  -> VAL f20.5
```

## Why Tau?

**The interval model has algebraic structure.** Half-open intervals `[start, end)` form a monoid under concatenation: `[a,b) + [b,c) = [a,c)`. Adjacent intervals tile cleanly, boundary ownership is unambiguous, and the structure composes. This is not an implementation detail — it is the reason the correctness properties are provable rather than assumed.

**Conflict resolution is a deterministic rule, not a policy.** Every layer has a monotonically increasing ID. At any timestamp, the layer with the highest ID wins. There is no configuration, no per-lens strategy, no coordination between writers. Two concurrent appends to the same interval both succeed; the later one wins at query time.

**Compaction is a normalisation.** The sweep-line algorithm reduces N layers to one canonical layer. Every `AT`, `RANGE`, and `REDUCE` result is identical before and after — this equivalence is the core invariant checked by Tau's property-based test suite. Compaction reduces query cost; it does not change what queries return.

**Derived lenses are function composition.** `DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0` compiles an expression into a lazy closure at definition time. Closures compose: a lens derived from other derived lenses chains their closures. Rolling aggregations like `avg(cpu, -600, 0)` are first-class expression nodes, usable anywhere. Cycle detection runs at `DERIVE` time by walking the dependency graph.

**Correctness is tested, not claimed.** The property-based test suite (Hegel/Hypothesis) checks algebraic invariants across hundreds of randomised inputs per property. The deterministic simulation tester (`dst`) drives every transport x auth x WAL configuration against a reference oracle, injecting faults across hundreds of millions of simulated operations. If there is a divergence, `dst` finds it, prints the seed, and tells you exactly how to reproduce it.

---

## Get started

```bash
cargo install --git https://github.com/bxrne/tau tau
tau                   # in-memory server on 127.0.0.1:7070
```

Or with Docker:

```bash
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Connect with any TCP client, or use the included REPL:

```bash
ctl
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE demo
τ: CREATE LENS cpu int
τ: APPEND LENS cpu 0 60 45, 60 120 72
τ: AT LENS cpu 30
VAL i45
```

[Full quick-start tutorial](/docs/tutorials/local/)

---

## Key features

| Feature | What it means |
|---------|--------------|
| **Half-open interval monoid** | `[start, end)` intervals tile cleanly; concatenation is associative; no boundary ambiguity |
| **Layer total order** | Every append gets a monotonically increasing ID; newest-layer-wins is a deterministic rule, not a policy |
| **Compaction normalisation** | Sweep-line reduces N layers to one canonical layer; query results are provably identical before and after |
| **Derived lenses** | Lazy closure composition; cycles detected at `DERIVE` time; rolling window aggregations are first-class |
| **Property-based tests** | Hegel/Hypothesis verifies algebraic invariants across hundreds of randomised inputs per property |
| **Deterministic simulation** | `dst` runs every config combination against a reference oracle; faults injected; reproducible by seed |
| **TLS + auth** | Optional TLS and Argon2id authentication with per-database CRUDA grants |
| **WAL durability** | Per-statement fsync, AES-256-GCM encryption at rest, schema DDL persisted and replayed |

---

## Explore

- [Overview](/docs/overview/): the data model and its algebraic properties
- [TauQL Reference](/docs/tauql/): every statement and operator
- [How it works](/docs/how-it-works/): storage, WAL, compaction, concurrency
- [Testing](/docs/testing/): property-based tests and the deterministic simulation tester
- [Examples](/docs/examples/): worked queries against real datasets
- [Tutorials](/docs/tutorials/local/): local, Docker, embedded
- [Configuration](/docs/configuration/): all server flags and environment variables
- [Containers](/docs/containers/): Docker stack with Prometheus and Grafana

---

*Tau is open source under the [PolyForm Noncommercial License](https://github.com/bxrne/tau/blob/master/LICENSE). Free for personal use, research, and education.*
