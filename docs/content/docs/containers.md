+++
title = "Containers"
date = 2026-05-28
weight = 60
template = "page.html"
+++

Tau ships a production Docker stack: **Tau + Prometheus + Grafana**, wired up out of the box, and a Helm chart for Kubernetes deployments.

---

## Docker (standalone)

```bash
docker pull ghcr.io/bxrne/tau:latest

# Ephemeral, in-memory, defaults
docker run --rm -p 7070:7070 ghcr.io/bxrne/tau:latest

# With a config file and a named volume for data persistence (WAL + users DB)
docker run -d --name tau \
  -p 7070:7070 -p 9100:9100 \
  -v tau_data:/data \
  -v "$PWD/tau-config.toml:/data/tau-config.toml:ro" \
  -e TAU_ENCRYPTION_KEY="$(openssl rand -hex 32)" \
  ghcr.io/bxrne/tau:latest --config /data/tau-config.toml
```

The image is a `scratch` base with a single static musl binary; its entrypoint is
`/tau` and it defaults to `--config /data/tau-config.toml`. The `/data` directory is a
declared volume — mount a named volume or host path there to persist the WAL and
the users database across restarts.

| env var | purpose |
|---------|---------|
| `TAU_ENCRYPTION_KEY` | 64 hex chars (32 bytes); enables AES-256-GCM encryption at rest for the WAL and disk store |

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

Connect with `tauctl` (install it from a release binary or `cargo install --git https://github.com/bxrne/tau tauctl`):

```bash
tauctl
τ connect prod 127.0.0.1:7070
τ AUTH admin <your password from tau-config.toml>
τ CREATE DATABASE sensors
```

Open Grafana: `http://localhost:3000` (credentials from your `.env`).

---

## Compose services

`container/docker-compose.yml` defines three services on an internal bridge network:

| service | image | role |
|---------|-------|------|
| `tau` | `ghcr.io/bxrne/tau` | The database. Mounts `tau_data:/data` for the WAL and users DB. Exposes 7070 (TauQL) and 9100 (metrics). |
| `prometheus` | `prom/prometheus` | Scrapes `tau:9100/metrics`, evaluates `prometheus/alerts.yml`. Web UI on 9090. |
| `grafana` | `grafana/grafana` | Dashboards from `grafana/provisioning`. Web UI on 3000. Credentials from `GRAFANA_USER`/`GRAFANA_PASSWORD`. |

Tag defaults to `latest`; pin with `TAU_IMAGE_TAG=v0.1.0` in `.env`.

Host-visible ports default to `127.0.0.1` so nothing is exposed beyond loopback unless you override the bind addresses in `.env`:

| port | service | purpose |
|------|---------|---------|
| 7070 | tau | TauQL query port (TCP, optionally TLS) |
| 9100 | tau | Prometheus `/metrics` and `/healthz` |
| 9090 | prometheus | Prometheus web UI |
| 3000 | grafana | Grafana dashboards |

---

## Configuration

The Docker stack mounts `container/tau-config.toml` into the container as
`/data/tau-config.toml` and passes `--config /data/tau-config.toml` to the server.
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
tauctl
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

## Benchmarks

`container/docker-compose.bench.yml` runs the `bench` crate's `benchtau` binary in a
resource-capped, read-only container and writes JSON results to a volume. It needs no
separate `tau` service - it spawns its own ephemeral server in-process for the wire layer.

```bash
docker compose -f container/docker-compose.bench.yml up
```

See [Benchmarks](/docs/benchmarks/) for the workloads, config grid, and reproducibility
contract, and `container/README.md` for the caps and environment variables.

## Production hardening checklist

- [ ] Set strong `[auth] password` in `tau-config.toml` and `GRAFANA_PASSWORD` in `.env`
- [ ] Set `TAU_ENCRYPTION_KEY` for encryption at rest
- [ ] Enable TLS in `tau-config.toml` with real certificates
- [ ] Put a reverse proxy (nginx/Caddy/Traefik) in front of Grafana
- [ ] Configure Alertmanager and on-call routing
- [ ] Back up the `tau_data` volume on schedule (WAL + `users.db`)
- [ ] Pin `TAU_IMAGE_TAG` to a release tag rather than `latest` in production

---

## Kubernetes / Helm

Tau ships a Helm chart (`container/helm/tau/`) published to the GHCR OCI registry alongside each release.

### Prerequisites

- A Kubernetes cluster (minikube, k3s, or any cloud provider)
- `helm` ≥ 3.8 (for OCI registry support)
- `kubectl` configured for your cluster

### Install

```bash
# From the published OCI chart
helm install tau oci://ghcr.io/bxrne/charts/tau --version 0.1.4

# Or from the local chart (useful during development)
helm install tau container/helm/tau \
  --set service.type=LoadBalancer
```

The chart creates:
- A **StatefulSet** with one replica and a stable pod name (`tau-0`)
- A **PersistentVolumeClaim** for `/data` (WAL + users database)
- A **ConfigMap** rendered from `values.yaml` and mounted as `/etc/tau/tau-config.toml`
- A **Service** exposing ports 7070 (protocol) and 9100 (metrics)

### Values reference

| key | default | description |
|-----|---------|-------------|
| `image.tag` | chart `appVersion` | Image tag to deploy |
| `service.type` | `ClusterIP` | `ClusterIP` or `LoadBalancer` |
| `service.port` | `7070` | Tau protocol port |
| `service.metricsPort` | `9100` | Prometheus metrics port |
| `storage.size` | `1Gi` | PVC size for `/data` |
| `storage.storageClassName` | `""` | Storage class (empty = cluster default) |
| `config.logLevel` | `info` | Server log level |
| `config.compactThreshold` | `8` | Layers before compaction |
| `config.wal.enabled` | `true` | Enable WAL |
| `config.wal.path` | `/data/tau.wal` | WAL file path |
| `config.auth.enabled` | `false` | Enable authentication |
| `config.auth.username` | `""` | Bootstrap admin username |
| `config.auth.password` | `""` | Bootstrap admin password |
| `config.auth.usersFile` | `/data/users.db` | Persistent users file |
| `config.tls.enabled` | `false` | Enable TLS |
| `config.limits.maxConnections` | `1024` | Connection cap |
| `config.limits.idleTimeoutSecs` | `300` | Idle connection timeout |

### External access (LoadBalancer)

Tau uses raw TCP, not HTTP — a standard Ingress controller cannot route to it. Set `service.type=LoadBalancer` to provision an external IP from your cloud provider:

```bash
helm install tau oci://ghcr.io/bxrne/charts/tau --version 0.1.4 \
  --set service.type=LoadBalancer \
  --set config.auth.enabled=true \
  --set config.auth.username=admin \
  --set config.auth.password=changeme

# Wait for the external IP
kubectl get svc tau --watch
```

**Local cluster (minikube):** `minikube service tau --url` returns the accessible address without requiring a cloud load balancer.

### Connect with tauctl

```bash
ADDR=$(kubectl get svc tau -o jsonpath='{.status.loadBalancer.ingress[0].ip}')
tauctl
τ connect k8s ${ADDR}:7070
```

### Upgrade

```bash
helm upgrade tau oci://ghcr.io/bxrne/charts/tau --version <new-version>
```

The StatefulSet performs a rolling update. The PVC is preserved across upgrades.

---

## Building locally

```bash
docker build \
  --build-arg RUST_VERSION=1.94.1 \
  --build-arg BUILD_PROFILE=release \
  -f container/Dockerfile \
  -t tau:local .

docker run --rm -p 7070:7070 \
  -v $PWD/container/tau-config.toml:/data/tau-config.toml:ro \
  tau:local --config /data/tau-config.toml
```
