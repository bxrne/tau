+++
title = "Tau"
sort_by = "date"
paginate_by = 10
template = "index.html"
page_template = "page.html"
+++

A time-series database built on immutable, layered temporal intervals.

Every value in Tau is a fact that was true from time A to time B. Corrections append on top of existing data. Old records stay intact. The newest layer wins at query time. Write-write conflicts disappear entirely.

```
CREATE DATABASE sensors
CREATE LENS temperature float
APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
AT LENS temperature 1800        -> VAL f18.5
RANGE LENS temperature 0 7200  -> RANGE 2; 0:3600:f18.5; 3600:7200:f21
REDUCE LENS temperature 0 7200 USING avg  -> VAL f19.75
```

---

## Why Tau?

**Correction is the natural state of real data.** Sensor readings drift. Financial prices get restated. Audit logs need amendments. Traditional databases model these as mutations: update in place and the old value is gone. Tau models them as what they are: a newer layer superseding an older one. Nothing is lost. Everything is queryable.

**No write-write conflicts.** Every write is an append. Two concurrent clients writing to the same lens at the same time will both succeed. Resolution happens lazily at query time: newest layer wins. Out-of-order streams ingest without locking or coordination.

**Derived lenses are lazy and live.** `DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0` creates a virtual lens that evaluates its expression on demand. Nothing is materialised. It stays current because it re-evaluates on every query.

**Rolling window aggregations in expressions.** `avg(cpu, -600, 0)` inside a `DERIVE` evaluates the time-weighted average of `cpu` over the 600-unit window ending at the query point. Use it to build threshold alerts, smoothed baselines, and anomaly detectors as first-class database objects.

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
| **Immutable layers** | Corrections append; old data is never overwritten |
| **Newest-layer-wins** | Query resolution is deterministic: the most recent layer covering a timestamp always wins |
| **Derived lenses** | Lazy computed views over any expression; cycles detected at `DERIVE` time |
| **Rolling aggregations** | `avg`, `min`, `max`, `sum`, `count` over relative windows, first-class in expressions |
| **TLS + auth** | Optional TLS (PEM or ephemeral self-signed) and Argon2id authentication with per-database CRUDA grants |
| **WAL durability** | Per-statement fsync, AES-256-GCM encryption at rest |
| **Prometheus metrics** | Per-statement counters, latency histograms, process gauges |
| **DST tested** | Deterministic simulation tester covers compaction, WAL replay, auth, and fault injection across hundreds of millions of simulated operations |

---

## Explore

- [Overview](/docs/overview/): data model, internals, and design philosophy
- [TauQL Reference](/docs/tauql/): complete language reference
- [Examples](/docs/examples/): worked queries against real datasets
- [Tutorials](/docs/tutorials/local/): step-by-step for local, Docker, and embedded use
- [Configuration](/docs/configuration/): all server flags and environment variables
- [Containers](/docs/containers/): Docker stack with Prometheus and Grafana

---

*Tau is open source under the [PolyForm Noncommercial License](https://github.com/bxrne/tau/blob/master/LICENSE). Free for personal use, research, and education.*
