+++
title = "Local Tutorial"
date = 2026-05-28
template = "page.html"
+++

Run Tau locally from source. This tutorial assumes you have Rust installed.

**Prerequisites:** Rust 1.94.1 (`rustup` recommended), `git`, a terminal.

---

## 1. Clone and build

```bash
git clone https://github.com/bxrne/tau
cd tau
cargo build --release
```

The toolchain version is pinned in `rust-toolchain.toml`; `rustup` picks it up automatically.

This produces three binaries in `target/release/`:

| binary | purpose |
|--------|---------|
| `tau` | TCP server |
| `ctl` | interactive REPL |
| `dst` | deterministic simulation tester |

---

## 2. Start the server

```bash
# In-memory mode (data lost on restart)
cargo run --release

# With write-ahead log (durable across restarts)
cargo run --release -- --wal -w /tmp/tau.wal
```

The server listens on `127.0.0.1:7070` by default. You should see:

```
2024-01-01T12:00:00Z INFO tau: listening on 127.0.0.1:7070
```

---

## 3. Connect with the REPL

Open a second terminal:

```bash
cargo run --release --bin ctl
```

You'll see the `τ:` prompt. Start querying:

```
τ: connect demo 127.0.0.1:7070
connected to 127.0.0.1:7070 as demo (plain)
[ok in 1.2ms]

τ: CREATE DATABASE sensors
OK
[ok in 0.3ms]

τ: CREATE LENS temperature float
OK
[ok in 0.1ms]

τ: APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
OK
[ok in 0.2ms]

τ: AT LENS temperature 1800
VAL f18.5
[ok in 0.08ms]

τ: RANGE LENS temperature 0 7200
RANGE 2; 0:3600:f18.5; 3600:7200:f21
[ok in 0.1ms]

τ: REDUCE LENS temperature 0 7200 USING avg
VAL f19.75
[ok in 0.1ms]

τ: BATCH APPEND LENS temperature { 7200 10800 19.0 ; 10800 14400 23.5 }
OK
[ok in 0.1ms]

τ: HISTORY LENS temperature
LAYERS 2; 1:0:0:7200:2 2:0:7200:14400:2
[ok in 0.05ms]
```

---

## 4. Load the example data

The repo ships three CSV datasets in `examples/data/`:

```
τ: CREATE DATABASE metrics
τ: CREATE LENS cpu int
τ: CREATE LENS pressure float
τ: CREATE LENS requests int
τ: load cpu examples/data/cpu-load.csv
loaded 1440 rows into cpu (6 chunks)
τ: load pressure examples/data/pressure.csv
loaded 288 rows into pressure (2 chunks)
τ: load requests examples/data/requests.csv
loaded 720 rows into requests (3 chunks)
```

Run some queries (all timestamps in seconds; noon = 43200):

```
τ: AT LENS cpu 43200
VAL i73

τ: REDUCE LENS cpu 0 86400 USING avg
VAL f36.77

τ: DERIVE LENS cpu_smooth AS avg(cpu, -600, 0)
τ: AT LENS cpu_smooth 43200
VAL f52.3
```

See [Examples](/docs/examples/) for more worked queries.

---

## 5. Enable authentication

Restart the server with auth:

```bash
cargo run --release -- \
  --wal -w /tmp/tau.wal \
  --auth --username admin --password s3cr3t
```

In the REPL, authenticate when connecting:

```
τ: connect prod 127.0.0.1:7070 admin s3cr3t
connected to 127.0.0.1:7070 as prod (plain)
[ok in 15ms]
```

Or separately:

```
τ: connect prod 127.0.0.1:7070
τ: auth admin s3cr3t
OK
```

Create a read-only user:

```
τ: CREATE USER alice PASSWORD "hunter2"
τ: GRANT R ON sensors TO alice
τ: SHOW GRANTS alice
GRANTS 1; alice sensors:R
```

See [Permissions](/docs/permissions/) for the full CRUDA bitmap and per-statement requirements.

---

## 6. Enable TLS

```bash
cargo run --release -- --tls    # ephemeral self-signed cert (dev only)
```

```
τ: connect dev 127.0.0.1:7070 tls
connected to 127.0.0.1:7070 as dev (TLS)
```

For production, provide real cert and key files:

```bash
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

---

## 7. Enable Prometheus metrics

```bash
cargo run --release -- --metrics-port 9090
```

```bash
curl http://127.0.0.1:9090/healthz    # → ok
curl http://127.0.0.1:9090/metrics    # → Prometheus text format
```

---

## 8. Run the simulation tester

Verify correctness before relying on any new data:

```bash
# Fast embedded mode (30 seconds, no server required)
cargo run --release --bin dst -- --quick

# Full simulation across all transport/auth/WAL combinations
cargo run --release --bin dst
```

On success you'll see a table of `PASS` results. On failure, the seed is printed for reproduction.

---

## Next steps

- [TauQL Reference](/docs/tauql/): all statements and expressions
- [Permissions](/docs/permissions/): CRUDA bitmap and per-statement requirements
- [Configuration](/docs/configuration/): all flags and environment variables
- [Examples](/docs/examples/): more queries against the example datasets
