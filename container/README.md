# Tau DB container stack

Production Docker stack: Tau DB + Prometheus + Grafana.

## Quick start

```sh
cd container

# Copy config templates and fill in secrets/settings
cp .env.example .env
# Edit tau-config.toml: set [auth] username/password and other settings
$EDITOR tau-config.toml

# Start the stack
docker compose up -d

# Connect via tauctl
tauctl
# τ connect prod 127.0.0.1:7070
# τ AUTH admin <your password from tau-config.toml>

# Open Grafana
open http://localhost:3000   # login: GRAFANA_USER / GRAFANA_PASSWORD from .env
```

## Configuration

All server settings live in **`tau-config.toml`** (mounted as `/data/tau-config.toml`
inside the container). Edit it before starting the stack:

```toml
bind = "0.0.0.0:7070"
log_level = "info"
compact_threshold = 8

[wal]
enabled = true
path = "/data/tau.wal"

[auth]
enabled = true
username = "admin"
password = "changeme_use_a_strong_password"
users_file = "/data/users.db"

[metrics]
port = 9100

[limits]
max_connections = 1024
idle_timeout_secs = 300
```

Only secrets and infrastructure overrides (bind interfaces, resource limits,
encryption key) belong in `.env`. Copy `.env.example` to `.env` and fill in:

| variable | purpose |
|----------|---------|
| `TAU_ENCRYPTION_KEY` | 64 hex chars; enables AES-256-GCM at-rest encryption |
| `GRAFANA_PASSWORD` | Grafana admin password (required) |
| `TAU_IMAGE_TAG` | Pin to a release tag, e.g. `v0.1.3` |

## Pulling the image from GHCR

```sh
docker pull ghcr.io/bxrne/tau:latest
# Pin to a release tag for reproducibility
docker pull ghcr.io/bxrne/tau:v0.1.0
```

`docker-compose.yml` uses `ghcr.io/bxrne/tau:${TAU_IMAGE_TAG:-latest}` by default.

## Ports (host-visible)

| Port  | Service    | Purpose                                            |
|-------|------------|----------------------------------------------------|
| 7070  | tau        | TauQL query port (TCP, optionally TLS)             |
| 9100  | tau        | Prometheus `/metrics` and `/healthz` HTTP endpoint |
| 9090  | prometheus | Prometheus web UI                                  |
| 3000  | grafana    | Grafana dashboards                                 |

All ports default to `127.0.0.1`. Override `TAU_BIND_ADDR`, `TAU_METRICS_BIND_ADDR`,
`PROM_BIND_ADDR`, `GRAFANA_BIND_ADDR` in `.env` to expose externally.

## TLS

1. Edit `tau-config.toml`:
   ```toml
   [tls]
   enabled = true
   cert = "/data/tls/server.crt"
   key  = "/data/tls/server.key"
   ```
2. Uncomment the TLS volume mounts in `docker-compose.yml`.
3. Connect with tauctl: `connect prod 127.0.0.1:7070 tls`.

For development, set `enabled = true` with no `cert`/`key` — the server
generates an ephemeral self-signed cert at startup.

## Encryption at rest

```sh
openssl rand -hex 32   # generate key
```

Set `TAU_ENCRYPTION_KEY` in `.env`. WAL/Disk files written with this key are
AES-256-GCM encrypted and cannot be read without it.

## Prometheus alerts

`prometheus/alerts.yml` includes rules for: server down, high error rate,
auth brute-force, high append/read latency, high memory, connection rejections.

## Grafana dashboard

UID `tau-db-prod` at `http://localhost:3000/d/tau-db-prod`.

Rows: Overview · Throughput · Latency · Security · Resources.

## Healthcheck

The runtime base image is `scratch` — no shell, no `curl`. Use Prometheus:

- `up{job="tau"}` = 1 means the server is reachable.
- `GET /healthz` on the metrics port for Kubernetes/Nomad liveness probes.

## Benchmarks

`docker-compose.bench.yml` runs the `bench` crate's `benchtau` binary in a resource-capped,
read-only, `cap_drop: ALL` container and writes a JSON results file to the `bench_results`
volume. It builds its own ephemeral `tau` server in-process for the wire layer, so no separate
`tau` service is required.

```sh
docker compose -f container/docker-compose.bench.yml up
docker compose -f container/docker-compose.bench.yml run --rm bench
```

| Variable | Purpose | Default |
|----------|---------|---------|
| `TAU_BENCH_PRESET` | Config-grid preset (`quick`, `security`, `storage`, `full`) | `quick` |
| `TAU_BENCH_SCALE` | Measured operations per workload | `1000` |
| `TAU_BENCH_SEED` | RNG seed (deterministic workloads) | `42` |
| `TAU_BENCH_CPU_LIMIT` | CPU limit passed to `deploy.resources.limits.cpus` | `1.0` |
| `TAU_BENCH_MEM_LIMIT` | Memory limit passed to `deploy.resources.limits.memory` | `512M` |

The output JSON has `seed`, `scale`, and a `results` array with `workload`, `cell` (the
config-grid cell name, e.g. `tls`, `auth`, `disk_wal`), `layer` (`engine` or `wire`), `ops`,
`throughput_ops_sec`, `p50_us`, and `p99_us`. The TLS and encryption-at-rest sections above
correspond to the `tls`, `auth`, and `encryption` cells in the grid — see
[`crates/bench/README.md`](../crates/bench/README.md) for the full grid definition and the
[benchmarks docs](https://tau.bxrne.com/docs/benchmarks/) for methodology, caps, and published
numbers.

## Production hardening checklist

- [ ] Set strong `[auth] password` in `tau-config.toml` and `GRAFANA_PASSWORD` in `.env`
- [ ] Set `TAU_ENCRYPTION_KEY` for encryption at rest
- [ ] Enable TLS with real certificates
- [ ] Put a reverse proxy in front of Grafana with HTTPS
- [ ] Configure Alertmanager and on-call routing
- [ ] Back up the `tau_data` volume on schedule
- [ ] Pin `TAU_IMAGE_TAG` to a release tag
