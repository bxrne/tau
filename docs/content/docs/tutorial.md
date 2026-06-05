+++
title = "Tutorial: Sensor Drift Correction"
date = 2026-06-02
template = "page.html"
+++

This walkthrough shows Tau's core value proposition end-to-end: recording a time series, receiving a correction, querying both the old and new state, and understanding how compaction preserves all results.

The scenario: a temperature sensor was miscalibrated for the first hour of a deployment. The vendor issues a correction an hour later. You need to record both the original readings and the corrected ones, and be able to query either.

---

## Prerequisites

Install the server and client (see [the README](https://github.com/bxrne/tau#install) for release binaries, `cargo install`, and Docker):

```bash
cargo install --git https://github.com/bxrne/tau tau
cargo install --git https://github.com/bxrne/tau tauctl
```

Start the server:

```bash
tau
```

In a second terminal, open the client:

```bash
tauctl
```

---

## Step 1 — Set up the database

```
τ: connect local 127.0.0.1:7070
τ: CREATE DATABASE sensors
→ OK

τ: CREATE LENS temperature float
→ OK
```

The `CREATE DATABASE` call sets `sensors` as the active database. All subsequent lens operations target it.

---

## Step 2 — Record the original readings

The sensor reported data from hour 0 to hour 2 (timestamps in seconds):

```
τ: APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
→ OK
```

This writes one layer with two adjacent intervals. Query a point in each interval:

```
τ: AT LENS temperature 1800
→ VAL f18.5

τ: AT LENS temperature 5400
→ VAL f21
```

Inspect the full range:

```
τ: RANGE LENS temperature 0 7200
→ RANGE 2; 0:3600:f18.5; 3600:7200:f21
```

Compute the time-weighted average:

```
τ: REDUCE LENS temperature 0 7200 USING avg
→ VAL f19.75
```

---

## Step 3 — Apply a correction

The vendor confirms the first hour was reading 1.5 °C low. Issue a correction for `[0, 3600)`:

```
τ: APPEND LENS temperature 0 3600 20.0
→ OK
```

This writes a second layer covering the same interval. Tau never touches the original data — the old layer still exists on disk. Query the same points again:

```
τ: AT LENS temperature 1800
→ VAL f20

τ: AT LENS temperature 5400
→ VAL f21
```

The corrected value wins at `t=1800` because layer 2 has a higher ID than layer 1 (newest-layer-wins). The second interval `[3600, 7200)` is unchanged — layer 2 doesn't cover it, so layer 1 still wins there.

```
τ: RANGE LENS temperature 0 7200
→ RANGE 2; 0:3600:f20; 3600:7200:f21

τ: REDUCE LENS temperature 0 7200 USING avg
→ VAL f20.5
```

The aggregate now reflects the correction.

---

## Step 4 — Audit the correction history

`HISTORY LENS` shows every layer that exists for the lens:

```
τ: HISTORY LENS temperature
→ LAYERS 2; 1:<ts>:0:7200:2 2:<ts>:0:3600:1
```

Each entry: `<layer_id>:<written_at_ms>:<min_start>:<max_end>:<tau_count>`.

To see what the lens looked like before the correction, query against a specific layer:

```
τ: AT LENS temperature 1800 LAYER 1
→ VAL f18.5
```

Layer 1 still carries the original reading. Nothing was overwritten.

---

## Step 5 — Add a derived lens

Derive a Fahrenheit lens from the corrected Celsius data:

```
τ: DERIVE LENS fahrenheit AS temperature * 9.0 / 5.0 + 32.0
→ OK

τ: AT LENS fahrenheit 1800
→ VAL f68

τ: REDUCE LENS fahrenheit 0 7200 USING avg
→ VAL f68.9
```

Derived lenses are lazy closures — nothing is materialised. Every query re-evaluates the expression against the current layer stack, so the derived lens always reflects the latest correction automatically.

Add a smoothed 10-minute rolling average:

```
τ: DERIVE LENS temp_smooth AS avg(temperature, -600, 0)
→ OK

τ: AT LENS temp_smooth 3000
→ VAL f20
```

---

## Step 6 — Batch a correction atomically

Suppose two intervals need correcting together. Use `START TRANSACTION` to apply them as a single atomic unit:

```
τ: START TRANSACTION
→ OK

τ: APPEND LENS temperature 0 1800 20.1
→ OK

τ: APPEND LENS temperature 1800 3600 19.9
→ OK

τ: COMMIT
→ OK
```

Both appends were buffered until `COMMIT`. No other connection saw partial state during the transaction. `ROLLBACK` would have discarded both.

---

## Step 7 — Compaction

After several corrections the lens accumulates layers. Compaction fires automatically when the layer count exceeds the configured threshold (default 8). It is also possible to trigger it manually by appending enough layers in a test environment.

After compaction:

```
τ: HISTORY LENS temperature
→ LAYERS 1; 4:<ts>:0:7200:3
```

The lens now has one canonical layer containing three taus: `[0,1800)`, `[1800,3600)`, `[3600,7200)`. Every previous `AT`, `RANGE`, and `REDUCE` result is identical — compaction is a provable normalisation, checked by property tests on every build.

---

## What to read next

- [TauQL Reference](/docs/tauql/) — all statements, operators, and response formats.
- [Overview](/docs/overview/) — the data model in depth.
- [How it works](/docs/how-it-works/) — compaction algorithm, WAL, concurrency.
- [Configuration](/docs/configuration/) — WAL encryption, TLS, auth, metrics.
