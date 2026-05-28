+++
title = "Containers"
date = 2026-05-28
template = "page.html"
+++

# Containers

Tau ships a production Docker stack: **Tau + Prometheus + Grafana**, wired up out of the box.

---

## Quick start

```bash
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest
```

For the full observability stack:

```bash
git clone https://github.com/bxrne/tau
cd tau/container
cp .env.example .env          # set TAU_PASSWORD and GRAFANA_PASSWORD at minimum
docker compose up -d
```

Connect:

```bash
cargo run --release --bin ctl
τ: connect prod 127.0.0.1:7070 admin <your TAU_PASSWORD>
τ: CREATE DATABASE sensors
```

Open Grafana: `http://localhost:3000` (default credentials from your `.env`).

---

## Environment variables

Configure the container stack in `.env`:

| variable | default | description |
|----------|---------|-------------|
| `TAU_IMAGE_TAG` | `latest` | Pin to a release tag, e.g. `v0.1.0` |
| `TAU_PASSWORD` | (required) | Bootstrap admin password |
| `TAU_ADDR` | `0.0.0.0:7070` | Bind address inside the container |
| `TAU_METRICS_PORT` | `9090` | Port for Prometheus scraping |
| `TAU_LOG_LEVEL` | `info` | `error` \| `warn` \| `info` \| `debug` \| `trace` |
| `TAU_ENCRYPTION_KEY` | (none) | 64 hex chars; enables AES-256-GCM encryption at rest |
| `TAU_CPU_LIMIT` | `1` | Docker CPU limit |
| `TAU_MEM_LIMIT` | `512m` | Docker memory limit |
| `GRAFANA_USER` | `admin` | Grafana admin username |
| `GRAFANA_PASSWORD` | (required) | Grafana admin password |

---

## TLS

To enable TLS:

1. Place your PEM cert and key in `container/certs/`.
2. In `.env`: set `TAU_TLS=--tls --tls-cert /certs/server.crt --tls-key /certs/server.key`.
3. Connect with tauctl using the `tls` keyword: `connect prod 127.0.0.1:7070 tls admin <pass>`.

For development without a real certificate, omit the cert/key paths: `TAU_TLS=--tls`. The server generates an ephemeral self-signed cert at startup; tauctl accepts it by design.

---

## Encryption at rest

```bash
# Generate a key
openssl rand -hex 32
```

Add to `.env`:

```
TAU_ENCRYPTION_KEY=<your 64-char hex key>
```

WAL entries written with this key are AES-256-GCM encrypted. The key is never stored; keep it in a secrets manager and inject it at runtime. A WAL file written with a key cannot be read without it.

---

## Prometheus alert rules

The stack ships with `prometheus/alerts.yml`:

| alert | severity | fires when |
|-------|----------|------------|
| `TauDown` | critical | scrape fails for 1 minute |
| `TauHighErrorRate` | warning | error rate > 5% for 5 minutes |
| `TauCriticalErrorRate` | critical | error rate > 25% for 2 minutes |
| `TauAuthBruteForce` | critical | > 20 failed auth/s for 2 minutes |
| `TauHighAppendLatency` | warning | p95 APPEND latency > 5 ms for 5 minutes |
| `TauHighReadLatency` | warning | p95 read latency > 2 ms for 5 minutes |
| `TauHighMemory` | warning | resident set > 768 MiB for 5 minutes |
| `TauConnectionRejections` | warning | any connection refused at the max-connections cap |

To route alerts, add an `alertmanager` service and configure `alerting:` in `prometheus.yml`.

---

## Grafana dashboard

Dashboard UID: `tau-db-prod`. Open at `http://localhost:3000/d/tau-db-prod`.

Panels:

- **Overview**: throughput, error rate, auth failures, rejected connections
- **Throughput**: statements/s split by type
- **Latency**: p50/p95/p99 per statement type
- **Security**: AUTH attempts vs failures
- **Resources**: RSS, virtual memory, open FDs, uptime

---

## Loading data into the container

When a CSV file lives on your local machine (not inside the container), use tauctl's `load` command to ship it client-side:

```bash
cargo run --release --bin ctl
τ: connect prod 127.0.0.1:7070 admin <pass>
τ: CREATE DATABASE metrics
τ: CREATE LENS cpu int
τ: load cpu examples/data/cpu-load.csv
loaded 1440 rows into cpu (6 chunks)
```

When the file is on a Docker volume, use server-side `COPY`:

```bash
docker run --rm \
  -v tau_data:/data \
  -v "$PWD/examples/data:/src:ro" \
  alpine cp /src/cpu-load.csv /data/cpu-load.csv

# then in tauctl:
τ: COPY LENS cpu FROM "/data/cpu-load.csv"
```

---

## Production hardening checklist

- [ ] Set strong `TAU_PASSWORD` and `GRAFANA_PASSWORD`
- [ ] Set `TAU_ENCRYPTION_KEY` for encryption at rest
- [ ] Mount real TLS certificates and set `TAU_TLS`
- [ ] Put a reverse proxy (nginx/Caddy/Traefik) in front of Grafana
- [ ] Configure Alertmanager and on-call routing
- [ ] Tune `TAU_CPU_LIMIT`, `TAU_MEM_LIMIT`, `--max-connections`, `--idle-timeout-secs`
- [ ] Back up the `tau_data` volume on schedule (WAL + users.json)
- [ ] Pin `TAU_IMAGE_TAG` to a release tag rather than `latest` in production

---

## Building locally

```bash
docker build \
  --build-arg RUST_VERSION=1.94.1 \
  --build-arg BUILD_PROFILE=release \
  -f container/Dockerfile \
  -t tau:local .

docker run --rm -p 7070:7070 -p 9090:9090 tau:local \
  0.0.0.0:7070 --auth --username admin --password s3cr3t --metrics-port 9090
```
