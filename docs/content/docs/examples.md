+++
title = "Examples"
date = 2026-07-02
template = "page.html"
+++

Short, copy-pasteable TauQL for common workloads. Every snippet is a `tauctl` session
(`τ:` is the prompt); keywords are UPPERCASE, type names and aggregate functions are lowercase.
Timestamps are opaque `i64` — pick a unit (here: seconds, unless noted) and stay consistent.

For the full grammar see the [TauQL reference](/docs/tauql/); for the data model see the
[overview](/docs/overview/).

---

## IoT — sensor telemetry with corrections

A fleet reports readings; a device is later recalibrated and its history restated. Nothing is
overwritten, and you can still ask what you believed before the fix.

```
τ: CREATE DATABASE fleet
τ: CREATE LENS temp_c float

# Ingest a run of readings as one atomic layer.
τ: BATCH APPEND LENS temp_c { 0 60 21.4 ; 60 120 21.9 ; 120 180 22.3 }
→ OK

# A recalibration says the first two minutes read 1.5 low — append a correction.
τ: APPEND LENS temp_c 0 120 22.9
→ OK

τ: AT LENS temp_c 30
→ VAL f22.9                       # newest layer wins

τ: AT LENS temp_c 30 AS OF <before-fix-ts>
→ VAL f21.4                       # what we believed before the correction

τ: HISTORY LENS temp_c            # audit trail of every write
→ LAYERS 2; 1:<ts>:0:180; 2:<ts>:0:120
```

Downsample and retain with a derived rolling average and a TTL:

```
# 5-minute time-weighted rolling average, computed lazily on read.
τ: DERIVE LENS temp_5m AS avg(temp_c, -300, 0)
τ: AT LENS temp_5m 180
→ VAL f22.5

# Keep only the last 24h of raw readings (seconds); older data reads as NIL.
τ: SET TTL LENS temp_c 86400
```

Per-device data as a **multi-dimensional** lens (axis 0 = time, axis 1 = device id):

```
τ: CREATE LENS reading float AXES (time, device)
τ: APPEND LENS reading [0 60] [1 2] 21.4, [0 60] [2 3] 24.1
→ OK

τ: AT LENS reading 30 1          # device 1 at t=30
→ VAL f21.4

τ: RANGE LENS reading 0 300 AT (2)   # sweep time for device 2 only
→ RANGE 1; 0:60:f24.1
```

---

## Observability — metrics and rollups

Store a counter/gauge per series; derive SLO-style rollups; aggregate over a window.

```
τ: CREATE DATABASE o11y
τ: CREATE LENS http_p99_ms int
τ: CREATE LENS error_rate float

τ: APPEND LENS http_p99_ms 0 60 180, 60 120 240, 120 180 210
τ: APPEND LENS error_rate  0 60 0.002, 60 120 0.015, 120 180 0.004

# Window aggregates (time-weighted where it matters).
τ: REDUCE LENS http_p99_ms 0 180 USING max
→ VAL i240
τ: REDUCE LENS error_rate 0 180 USING avg
→ VAL f0.007

# A derived "burn" signal combining two series (materialised, auto-refreshed).
τ: XDERIVE LENS burn AS error_rate + http_p99_ms OVER 0 180
τ: RANGE LENS burn 0 180
→ RANGE 3; 0:60:f180.002; 60:120:f240.015; 120:180:f210.004

# Filter a range to just the segments breaching an SLO.
τ: RANGE LENS http_p99_ms 0 180 WHERE http_p99_ms > 200
→ RANGE 2; 60:120:i240; 120:180:i210
```

Late-arriving and out-of-order samples are first class — just `APPEND` them; the newest write wins
at any overlap, and `AS OF` still reconstructs the pre-arrival view for reproducible alert
post-mortems.

---

## Backtesting — point-in-time correctness

Bitemporality is the backtester's dream: `AS OF` gives you exactly what was known at a past instant,
so a strategy can never accidentally see a *restated* price (look-ahead bias).

```
τ: CREATE DATABASE market
τ: CREATE LENS px float

# Prices stream in over the day (timestamps = seconds since open).
τ: APPEND LENS px 0 3600 100.0, 3600 7200 101.2, 7200 10800 100.8

# An exchange restatement corrects the 09:00–10:00 bar the next day.
τ: APPEND LENS px 0 3600 100.4
→ OK

# Live/current view uses the restated price…
τ: AT LENS px 1800
→ VAL f100.4

# …but a backtest "as of" the original trade day sees the price it actually traded on.
τ: AT LENS px 1800 AS OF <trade-day-ts>
→ VAL f100.0

τ: HISTORY LENS px            # every vintage of the series is retained
→ LAYERS 2; 1:<ts>:0:10800; 2:<ts>:0:3600
```

Spreads and signals compose as derived lenses; a cross-instrument grid uses axes:

```
# Pair spread, re-evaluated live against the latest corrections.
τ: CREATE LENS aapl float
τ: CREATE LENS msft float
τ: DERIVE LENS spread AS aapl - msft

# Or a single grid lens keyed by (time, instrument-id).
τ: CREATE LENS quote float AXES (time, instrument)
τ: APPEND LENS quote [0 3600] [1 2] 100.0, [0 3600] [2 3] 250.5
τ: RANGE LENS quote 0 3600 AT (1)     # instrument 1's tape
→ RANGE 1; 0:3600:f100
```

---

## Finance — Lua triggers for rolling stats

Bitemporality gives you point-in-time-correct prices. Lua triggers give you computation that fires automatically on write — Sharpe ratios, rolling stddev, position-keeping — without an external pipeline.

```
τ: CREATE DATABASE market
τ: CREATE LENS returns float
τ: CREATE LENS sharpe float

# A 24h rolling Sharpe ratio, recomputed on every write to returns.
τ: CREATE FUNCTION sharpe_24h ON WRITE LENS returns CAPS exec, range, clock
AS "
  local s, e = tau.clock_window(86400000)
  local rows = tau.range('returns', s, e)
  local n, sum, sum2 = 0, 0.0, 0.0
  for _, r in ipairs(rows) do
    n = n + 1; sum = sum + r.v; sum2 = sum2 + r.v * r.v
  end
  if n < 2 then return end
  local mean = sum / n
  local sd = math.sqrt(math.max(sum2 / n - mean * mean, 0))
  local sh = (sd > 0) and (mean / sd) or 0.0
  tau.exec(('APPEND LENS sharpe %d %d %f'):format(s, e, sh))
"
→ OK

# Append a return — the trigger fires, Sharpe is appended automatically.
τ: APPEND LENS returns 0 3600 0.0012
→ OK

τ: AT LENS sharpe 1800
→ VAL f0.012

# Correct the return — the trigger re-fires against the latest data.
τ: APPEND LENS returns 0 3600 0.0021
→ OK

τ: AT LENS sharpe 1800
→ VAL f0.021

# What did Sharpe look like before the correction?
τ: AT LENS sharpe 1800 AS OF <before-correction-ts>
→ VAL f0.012
```

The Sharpe lens is itself bitemporal — corrections to `returns` produce new layers in `sharpe`, and `AS OF` reconstructs what the risk system believed at any past moment. See [Lua Scripting](/docs/scripting/) for the full reference.

---

## Cheatsheet

| Goal | Statement |
|------|-----------|
| Record a fact over `[s, e)` | `APPEND LENS x s e v` |
| Atomic multi-fact batch | `BATCH APPEND LENS x { s e v ; … }` |
| Correct history (no overwrite) | `APPEND LENS x s e v'` (newest wins) |
| Value now | `AT LENS x t` |
| Value as-of a past write-time | `AT LENS x t AS OF ts` |
| Segments over a window | `RANGE LENS x s e` |
| Filtered window | `RANGE LENS x s e WHERE x > k LIMIT n` |
| Window aggregate | `REDUCE LENS x s e USING avg` |
| Lazy transform | `DERIVE LENS y AS <expr>` |
| Materialised transform | `XDERIVE LENS y AS <expr> [OVER s e]` |
| Rolling window in an expr | `avg(x, -300, 0)` |
| Audit provenance | `HISTORY LENS x` |
| Retention | `SET TTL LENS x secs` / `UNSET TTL LENS x` |
| Multi-dimensional lens | `CREATE LENS g t AXES (time, k)` → `APPEND LENS g [s e] [a b] v`, `AT LENS g t k`, `RANGE LENS g s e AT (k)` |
| Lua trigger on write | `CREATE FUNCTION f ON WRITE LENS x CAPS exec AS "…"` |
| Scheduled function | `CREATE FUNCTION f SCHEDULE EVERY 86400 CAPS exec AS "…"` |
| Call a function | `CALL FUNCTION f(args)` |
| List functions | `SHOW FUNCTIONS` |
