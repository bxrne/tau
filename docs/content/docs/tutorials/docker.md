+++
title = "Docker Tutorial"
date = 2026-05-28
template = "page.html"
+++

# Docker Tutorial

Run Tau with Prometheus and Grafana using Docker Compose. This tutorial produces a production-ready observability stack in under five minutes.

**Prerequisites:** Docker 24+, Docker Compose v2.

---

## 1. Get the stack

```bash
git clone https://github.com/bxrne/tau
cd tau/container
```

Or just pull the image directly:

```bash
docker pull ghcr.io/bxrne/tau:latest
```

---

## 2. Configure secrets

```bash
cp .env.example .env
```

Edit `.env` and set at minimum:

```env
TAU_PASSWORD=your_strong_password_here
GRAFANA_PASSWORD=another_strong_password
```

Other useful settings:

```env
TAU_IMAGE_TAG=latest          # or pin to a release: v0.1.0
TAU_ENCRYPTION_KEY=           # 64 hex chars for encryption at rest
TAU_LOG_LEVEL=info
```

---

## 3. Start the stack

```bash
docker compose up -d
```

This starts:

| service | port | purpose |
|---------|------|---------|
| `tau` | `7070` | TCP query server |
| `tau` | `9090` | Prometheus metrics |
| `prometheus` | `19090` | Prometheus UI |
| `grafana` | `3000` | Grafana dashboards |

Check everything is running:

```bash
docker compose ps
curl http://localhost:9090/healthz    # → ok
```

---

## 4. Connect and query

Install and run tauctl, or build from source:

```bash
cargo run --release --bin ctl

τ: connect prod 127.0.0.1:7070 admin <your TAU_PASSWORD>
connected to 127.0.0.1:7070 as prod (plain)
[ok in 8ms]

τ: CREATE DATABASE sensors
τ: CREATE LENS temperature float
τ: APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
τ: AT LENS temperature 1800
VAL f18.5
```

---

## 5. Load example data

Ship a CSV from your local machine into the containerised server:

```bash
τ: CREATE DATABASE metrics
τ: CREATE LENS cpu int
τ: load cpu examples/data/cpu-load.csv
loaded 1440 rows into cpu (6 chunks)
```

The `load` command reads the file from your local filesystem and ships it as batched `APPEND` statements over the active TCP connection. No file access is required on the server side.

Alternatively, copy the file into the Docker volume and use server-side `COPY`:

```bash
docker run --rm \
  -v container_tau_data:/data \
  -v "$PWD/examples/data:/src:ro" \
  alpine cp /src/cpu-load.csv /data/cpu-load.csv

# then:
τ: COPY LENS cpu FROM "/data/cpu-load.csv"
```

---

## 6. Open Grafana

Visit `http://localhost:3000` and log in with `admin` / `<your GRAFANA_PASSWORD>`.

The provisioned dashboard (`tau-db-prod`) shows:

- Statement throughput by type
- Latency histograms (p50/p95/p99)
- Error rates and auth failure rates
- Memory, file descriptors, uptime

---

## 7. Enable TLS

Create a `certs/` directory inside `container/` and place your PEM files there:

```bash
mkdir container/certs
cp server.crt server.key container/certs/
```

In `.env`:

```env
TAU_TLS=--tls --tls-cert /certs/server.crt --tls-key /certs/server.key
```

Restart:

```bash
docker compose up -d tau
```

Connect with TLS:

```
τ: connect prod 127.0.0.1:7070 tls admin <pass>
```

For dev without a real cert, `TAU_TLS=--tls` generates an ephemeral self-signed cert. tauctl accepts it by design.

---

## 8. Enable encryption at rest

Generate a key:

```bash
openssl rand -hex 32
```

Add to `.env`:

```env
TAU_ENCRYPTION_KEY=<64-char hex string>
```

Restart Tau:

```bash
docker compose up -d tau
```

All subsequent WAL entries are AES-256-GCM encrypted. Keep the key in a secrets manager; a WAL written with the key cannot be read without it.

---

## 9. Stop and reset

```bash
# Stop and keep data
docker compose down

# Stop and remove all data volumes
docker compose down -v
```

---

## Troubleshooting

**Tau fails to start:**

```bash
docker compose logs tau
```

Most common causes: port 7070 already in use, `.env` file missing, malformed `TAU_ENCRYPTION_KEY`.

**Cannot connect from tauctl:**

- Verify port mapping: `docker compose ps` should show `0.0.0.0:7070->7070/tcp`
- Check auth: use the password from `.env`

**Grafana shows no data:**

- Confirm Prometheus is scraping: `http://localhost:19090/targets`
- Wait 30 seconds after first connection for metrics to accumulate
