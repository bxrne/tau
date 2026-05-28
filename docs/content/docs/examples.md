+++
title = "Examples"
date = 2026-05-28
template = "page.html"
+++

Three real datasets shipped with the repository, with worked queries against each. The data lives in `examples/data/` and is committed to the repo so the examples are reproducible.

---

## Datasets

| file | type | rows | resolution | span | shape |
|------|------|------|------------|------|-------|
| `pressure.csv` | `float` | 288 | 5 min | 24 h | Barometric pressure (hPa), diurnal swing around 1013 |
| `cpu-load.csv` | `int` | 1440 | 1 min | 24 h | CPU utilisation %, quiet overnight, mid-day spike |
| `requests.csv` | `int` | 720 | 1 min | 12 h | Per-minute HTTP request counts, smooth ramp + a burst |

Every file uses contiguous, non-overlapping intervals: adjacent rows share a boundary timestamp, so a point lookup always lands inside exactly one interval.

---

## Loading the data

### Client-side load (your machine → remote server)

```bash
cargo run --release --bin ctl

τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE metrics
τ: CREATE LENS pressure float
τ: CREATE LENS cpu int
τ: CREATE LENS requests int
τ: load pressure examples/data/pressure.csv
loaded 288 rows into pressure (2 chunks)
τ: load cpu examples/data/cpu-load.csv
loaded 1440 rows into cpu (6 chunks)
τ: load requests examples/data/requests.csv
loaded 720 rows into requests (3 chunks)
```

### Server-side load (file already on server)

```
τ: COPY LENS cpu FROM "/path/to/examples/data/cpu-load.csv"
OK
```

---

## Point lookups

Timestamps are in seconds. `t=21600` is 06:00, `t=43200` is noon (12:00), `t=86400` is end-of-day.

```
τ: AT LENS cpu 43200
VAL i73

τ: AT LENS pressure 43200
VAL f1014.21

τ: AT LENS requests 21600
VAL i1374
```

---

## Aggregates over a window

```
τ: REDUCE LENS cpu 0 86400 USING count
VAL i1440                  ← one interval per minute, full 24 hours

τ: REDUCE LENS cpu 0 86400 USING avg
VAL f36.7694444444         ← time-weighted mean

τ: REDUCE LENS cpu 0 86400 USING min
VAL i5

τ: REDUCE LENS cpu 0 86400 USING max
VAL i82

τ: REDUCE LENS pressure 0 86400 USING avg
VAL f1014.1055555555       ← ~1014.1 hPa mean

τ: REDUCE LENS pressure 0 86400 USING min
VAL f1009.76

τ: REDUCE LENS pressure 0 86400 USING max
VAL f1018.54

τ: REDUCE LENS requests 0 43200 USING sum
VAL i904825                ← total requests in the first 12 hours

τ: REDUCE LENS requests 0 43200 USING max
VAL i2095                  ← peak minute (the burst)
```

---

## Range scan

```
τ: RANGE LENS cpu 35000 37000
RANGE 34; 35000:35040:i40; 35040:35100:i38; ...; 36960:37000:i47
```

34 segments because every minute boundary inside the window is a change point. Adjacent segments with identical values are merged automatically; a long flat run produces a single segment.

---

## Range with `WHERE` filter

```
τ: RANGE LENS cpu 0 86400 WHERE cpu > 75
RANGE 78; 44340:44400:i76; 44640:44700:i76; ...; 53880:53940:i78
```

78 segments, all in the mid-day window when the spike pushed CPU above 75%. The filter is a plain expression; any operator (`< <= == != >= >`, `&& ||`, arithmetic) works.

---

## Derived lenses

A derived lens is a lazy expression over other lenses. Nothing is materialised: every query re-evaluates the expression on the current data.

### Rolling 10-minute average

```
τ: DERIVE LENS cpu_smooth AS avg(cpu, -600, 0)
OK

τ: AT LENS cpu_smooth 43200
VAL f52.3           ← time-weighted mean of cpu over [42600, 43200)
```

`avg(cpu, -600, 0)` evaluates at time `t` as the time-weighted average of `cpu` over `[t-600, t)`.

### Threshold-derived boolean

```
τ: DERIVE LENS cpu_busy AS cpu > 70
OK

τ: AT LENS cpu_busy 43200
VAL b1              ← true at noon

τ: AT LENS cpu_busy 7200
VAL b0              ← false at 02:00
```

Range and reduce work on boolean lenses just like any other.

### Cross-lens arithmetic

```
τ: DERIVE LENS req_rate AS requests / 60
OK

τ: AT LENS req_rate 21600
VAL i22             ← ~22 requests/second at 06:00
```

### Rolling baseline anomaly detection

```
τ: DERIVE LENS cpu_hot AS cpu > avg(cpu, -1800, 0)
OK

τ: AT LENS cpu_hot 43200
VAL b1              ← instantaneous reading is above the 30-minute rolling mean
```

This is the canonical "is the signal above its own rolling baseline" pattern. Because adjacent same-value segments are merged, you get one segment per state-change rather than one per minute; ideal for alerting.

---

## Reset

```bash
# wipe demo state and start fresh
rm -rf /tmp/tau-demo
```

Docker stack:

```bash
cd container && docker compose down -v && docker compose up -d
```
