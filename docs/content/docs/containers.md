+++
title = "Containers"
date = 2026-05-28
template = "page.html"
+++

Tau ships a production Docker stack: **Tau + Prometheus + Grafana**, wired up out of the box.

---

## Quick start

```bash
docker pull ghcr.io/bxrne/tau:latest
docker run --rm -p 7070:7070 \
  -v $PWD/config.toml:/data/config.toml:ro \
  ghcr.io/bxrne/tau:latest --config /data/config.toml
```

For the full observability stack:

```bash
git clone https://github.com/bxrne/tau
cd tau/container

# Copy config templates and edit them
cp .env.example .env
# Edit tau-config.toml: set [auth] username/password and any other settings
$EDITOR tau-config.toml

docker compose up -d
```

Connect:

```bash
cargo run --release --bin ctl
τ connect prod 127.0.0.1:7070
τ AUTH admin <your password from tau-config.toml>
τ CREATE DATABASE sensors
```

Open Grafana: `http://localhost:3000` (credentials from your `.env`).

---

## Configuration

The Docker stack mounts `container/tau-config.toml` into the container as
`/data/config.toml` and passes `--config /data/config.toml` to the server.
Edit `tau-config.toml` directly for all server settings:

```toml
bind = "0.0.0.0:7070"
log_level = "info"

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

See [Configuration](../configuration/) for the full field reference.

---

## Environment variables (`.env`)

Only secrets and infrastructure overrides live in `.env`. Server settings
(bind address, auth credentials, WAL path, etc.) are in `tau-config.toml`.

| variable | default | description |
|----------|---------|-------------|
| `TAU_ENCRYPTION_KEY` | (none) | 64 hex chars; enables AES-256-GCM encryption at rest |
| `TAU_IMAGE_TAG` | `latest` | Pin to a release tag, e.g. `v0.1.3` |
| `TAU_BIND_ADDR` | `127.0.0.1` | Host interface for port 7070 |
| `TAU_METRICS_BIND_ADDR` | `127.0.0.1` | Host interface for port 9100 |
| `TAU_CPU_LIMIT` | `2.0` | Docker CPU limit |
| `TAU_MEM_LIMIT` | `512M` | Docker memory limit |
| `GRAFANA_USER` | `admin` | Grafana admin username |
| `GRAFANA_PASSWORD` | (required) | Grafana admin password |
| `GF_SERVER_ROOT_URL` | `http://localhost:3000` | Grafana external URL |

---

## TLS

1. Place your PEM cert and key on the host.
2. Edit `tau-config.toml`:
   ```toml
   [tls]
   enabled = true
   cert = "/data/tls/server.crt"
   key  = "/data/tls/server.key"
   ```
3. Uncomment the TLS volume mounts in `docker-compose.yml`:
   ```yaml
   - /etc/tau/tls/server.crt:/data/tls/server.crt:ro
   - /etc/tau/tls/server.key:/data/tls/server.key:ro
   ```
4. Connect with tauctl using the `tls` keyword: `connect prod 127.0.0.1:7070 tls`.

For development, set `enabled = true` with no `cert`/`key` paths — the server
generates an ephemeral self-signed cert at startup; tauctl accepts it by design.

---

## Encryption at rest

```bash
openssl rand -hex 32   # generate a 32-byte key
```

Set in `.env`:

```
TAU_ENCRYPTION_KEY=<your 64-char hex key>
```

WAL entries written with this key are AES-256-GCM encrypted. Keep the key in a
secrets manager and inject it at runtime. A WAL written with a key cannot be
read without it.

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

---

## Grafana dashboard

Dashboard UID: `tau-db-prod`. Open at `http://localhost:3000/d/tau-db-prod`.

Panels: Overview · Throughput · Latency · Security · Resources.

---

## Loading data into the container

**Client-side (file on your laptop)** — use tauctl's `load` command:

```bash
cargo run --release --bin ctl
τ connect prod 127.0.0.1:7070
τ AUTH admin <pass>
τ CREATE DATABASE metrics
τ CREATE LENS cpu int
τ load cpu examples/data/cpu-load.csv
loaded 1440 rows into cpu (6 chunks)
```

**Server-side (file on a Docker volume)** — stage the file then use `COPY`:

```bash
docker run --rm \
  -v tau_data:/data \
  -v "$PWD/examples/data:/src:ro" \
  alpine cp /src/cpu-load.csv /data/cpu-load.csv

# then in tauctl:
τ COPY LENS cpu FROM "/data/cpu-load.csv"
```

---

## Production hardening checklist

- [ ] Set strong `[auth] password` in `tau-config.toml` and `GRAFANA_PASSWORD` in `.env`
- [ ] Set `TAU_ENCRYPTION_KEY` for encryption at rest
- [ ] Enable TLS in `tau-config.toml` with real certificates
- [ ] Put a reverse proxy (nginx/Caddy/Traefik) in front of Grafana
- [ ] Configure Alertmanager and on-call routing
- [ ] Back up the `tau_data` volume on schedule (WAL + `users.db`)
- [ ] Pin `TAU_IMAGE_TAG` to a release tag rather than `latest` in production

---

## Building locally

```bash
docker build \
  --build-arg RUST_VERSION=1.94.1 \
  --build-arg BUILD_PROFILE=release \
  -f container/Dockerfile \
  -t tau:local .

docker run --rm -p 7070:7070 \
  -v $PWD/config.toml:/data/config.toml:ro \
  tau:local --config /data/config.toml
```
