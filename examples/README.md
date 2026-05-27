# Tau examples

Three curated CSVs and worked experiments against them. The data lives in [`data/`](data/) and is committed (not generated at runtime) so the examples are reproducible. The numbers in the *Measured performance* tables were captured by running `cargo run --release --bin ctl` against a freshly-started `tau` server on Linux against the actual files in this directory.

## Datasets

All files follow the same `start,end,value` schema (one row = one tau), with `#` for comments and blank lines ignored. They are deliberately small enough to commit to git, but large enough to expose meaningful query behaviour: a single 24-hour CPU lens with 1440 1-minute taus is enough to exercise compaction, multi-chunk loads, and rolling aggregations.

| file                                | type    | rows  | bytes  | resolution | span        | shape                                                |
|-------------------------------------|---------|-------|--------|------------|-------------|------------------------------------------------------|
| [`data/pressure.csv`](data/pressure.csv)       | `float` | 288   | ~6 KiB | 5 min      | 24 h        | Barometric pressure (hPa), diurnal swing around 1013. |
| [`data/cpu-load.csv`](data/cpu-load.csv)       | `int`   | 1440  | ~21 KiB | 1 min     | 24 h        | CPU utilisation %. Quiet overnight, mid-day spike.    |
| [`data/requests.csv`](data/requests.csv)       | `int`   | 720   | ~12 KiB | 1 min     | 12 h        | Per-minute request counts. Smooth ramp + a burst.    |

Every file is *contiguous and non-overlapping*: adjacent rows share their boundary timestamp so a single point lookup always lands inside exactly one tau.

## Loading the data

Two paths depending on where the file lives. **Both work; pick by location.**

### From your laptop into a remote / containerised server

```sh
cargo run --release --bin ctl
```

```text
τ: connect demo 127.0.0.1:7070 admin <YOUR_PASSWORD>
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

`load` reads the CSV on the **client** (your laptop), parses it locally, and ships chunked multi-tau `APPEND` statements over the already-authenticated TCP connection. The server never sees the file path.

### From the server's own filesystem

When the file is already in the server's view (embedded mode, Docker volume mount, mounted host directory), `COPY` reads it server-side in a single statement:

```text
τ: COPY LENS cpu FROM "/path/on/server/cpu-load.csv"
```

For the Docker stack, stage the file into the named volume first:

```sh
docker run --rm \
  -v container_tau_data:/data \
  -v "$PWD/examples/data:/src:ro" \
  alpine cp /src/cpu-load.csv /data/cpu-load.csv
```

Then in tauctl: `COPY LENS cpu FROM "/data/cpu-load.csv"`.

## Useful queries

After loading all three lenses, here is the experiment menu, with real returned values from the captured run. `t=21600` is 06:00, `t=43200` is noon (12:00), and `t=86400` is the end of the 24-hour window.

### Point lookups

```text
τ: AT LENS cpu 43200
VAL i73
τ: AT LENS pressure 43200
VAL f1014.21
τ: AT LENS requests 21600
VAL i1374
```

### Aggregates over a window

```text
τ: REDUCE LENS cpu 0 86400 USING count
VAL i1440                      # one tau per minute, full 24 h
τ: REDUCE LENS cpu 0 86400 USING avg
VAL f36.7694444444              # weighted by interval duration
τ: REDUCE LENS cpu 0 86400 USING min
VAL i5
τ: REDUCE LENS cpu 0 86400 USING max
VAL i82
τ: REDUCE LENS pressure 0 86400 USING avg
VAL f1014.1055555555            # ~1014.1 hPa over 24 h
τ: REDUCE LENS pressure 0 86400 USING min
VAL f1009.76
τ: REDUCE LENS pressure 0 86400 USING max
VAL f1018.54
τ: REDUCE LENS requests 0 43200 USING sum
VAL i904825                    # total requests in the first 12 h
τ: REDUCE LENS requests 0 43200 USING max
VAL i2095                      # peak minute (the burst)
```

### Range scan

```text
τ: RANGE LENS cpu 35000 37000
RANGE 34; 35000:35040:i40; 35040:35100:i38; ...; 36960:37000:i47
```

34 segments because every minute boundary inside the window is a change point. Tau automatically merges adjacent segments that share a value, which is why a long flat run would collapse into a single segment.

### Range with `WHERE` filter

```text
τ: RANGE LENS cpu 0 86400 WHERE cpu > 75
RANGE 78; 44340:44400:i76; 44640:44700:i76; ...; 53880:53940:i78
```

78 segments over the full day, all in the mid-day window when the spike pushed CPU above 75%. The filter is just an expression — any of the operators (`< <= == != >= >`, `&& ||`, arithmetic) can appear.

## Derived lens examples

A derived lens is a lazy expression over other lenses. Nothing is materialised: every query re-evaluates the expression on the current data.

### Rolling 10-minute average of CPU

```text
τ: DERIVE LENS cpu_smooth AS avg(cpu, -600, 0)
OK
τ: AT LENS cpu_smooth 43200
VAL f52.3                       # time-weighted mean of cpu over [42600, 43200)
```

`avg(cpu, -600, 0)` evaluates at time `t` as the time-weighted average of `cpu` over `[t-600, t)`. The derived lens's type is whatever the expression yields — here a `float` even though the base lens is `int`.

### Threshold-derived boolean

```text
τ: DERIVE LENS cpu_busy AS cpu > 70
OK
τ: AT LENS cpu_busy 43200
VAL b1                          # true at noon
τ: AT LENS cpu_busy 7200        # 02:00
VAL b0                          # false overnight
```

`cpu_busy` is type `bool`, derived from an `int` base. Range / reduce work on it just like any other lens; the boundaries come from the base lens's change points.

### Cross-lens arithmetic

```text
τ: DERIVE LENS req_rate_per_sec AS requests / 60
OK
τ: AT LENS req_rate_per_sec 21600
VAL i24                         # 1374 req/min / 60 ~= 22-24 req/s
```

Derived lenses can reference any other lens, including other derived lenses. Cycles are detected at `DERIVE LENS` time and rejected with `ExecError::CycleDetected`.

### Hot when above the rolling baseline

```text
τ: DERIVE LENS cpu_hot AS cpu > avg(cpu, -1800, 0)
OK
τ: AT LENS cpu_hot 43200
VAL b1                          # the instantaneous reading > 30-min mean
```

The canonical "is the signal above its own rolling baseline" pattern. Useful for alerting derivations because adjacent same-value segments are merged so you get one segment per state-change rather than one per minute.

## Measured performance

Captured on Linux running the `--release` build against a single connection over loopback TCP, WAL on, auth on. Numbers come from the server's own `/metrics` endpoint scraped after the run; they exclude network and parse cost. The run loaded all three datasets, defined four derived lenses, and executed every query listed above.

### Per-statement latency (executor time only)

| operation                 | n   | total time | mean per op                        |
|---------------------------|----:|-----------:|------------------------------------|
| `APPEND` (256-row chunk)  | 11  | 572 µs     | **52 µs/chunk** (~200 ns/tau)      |
| `AT` point lookup         | 6   | 24 µs      | **4 µs**                           |
| `RANGE` scan              | 2   | 264 µs     | **132 µs** (34- and 78-segment)    |
| `REDUCE` aggregate        | 9   | 491 µs     | **55 µs/agg** (some over 24 h)     |
| DDL (`CREATE`, `DERIVE`)  | 8   | 137 µs     | **17 µs**                          |

These are best-case in-memory + WAL-fsync numbers on a quiet machine. The 24-hour `cpu` lens (1440 rows) compacts down to a single canonical layer after the configured `--compact-threshold` (32) is crossed during the load.

### Process footprint after loading all three datasets

| gauge                          | value      |
|--------------------------------|------------|
| `tau_process_resident_bytes`   | 24.0 MiB   |
| `tau_process_open_fds`         | 8          |
| `tau_process_threads`          | 3          |
| `tau_connections_total`        | 1          |
| Total taus in store            | 2448       |

The process is small because there is no per-database storage engine: a single `Database<Value>` owns one `HashMap<lens, Vec<Layer>>` plus an optional WAL handle. Memory scales with the number of distinct live taus, not with the number of historical layers (after compaction).

### Chunk-size tradeoff for `load`

The default `load` chunk is 256 rows per `APPEND`. For the 1440-row `cpu-load.csv`:

| chunk size       | wire round-trips |
|-----------------:|------------------|
| 64               | 23               |
| **256 (default)**| **6**            |
| 1024             | 2                |

Larger chunks mean fewer round-trips and faster ingest; the only ceiling is the server's per-line size (line-oriented protocol, no hard cap but lines >1 MiB stress the buffered reader). For most CSVs the default is fine. Tune with `load <lens> <path> <chunk>`.

## Reset

```sh
# Wipe local demo state and start over
rm -rf /tmp/tau-demo
```

Or, for the Docker stack:

```sh
cd container && docker compose down -v && docker compose up -d
```
