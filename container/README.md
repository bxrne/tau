# Tau DB container stack

Production Docker stack: Tau DB + Prometheus + Grafana.

## Quick start

```sh
cd container

# Copy and fill in secrets
cp .env.example .env
$EDITOR .env                             # set TAU_PASSWORD and GRAFANA_PASSWORD at minimum

# Start the stack. The image is pulled from GHCR by default; if you want a
# local build instead, use `docker compose up -d --build`.
docker compose up -d

# Connect via tauctl (built locally - it lives in the same repo)
cargo run --release --bin ctl
# inside the REPL:
#   connect prod 127.0.0.1:7070 admin <your TAU_PASSWORD>

# Open Grafana
open http://localhost:3000              # login with GRAFANA_USER / GRAFANA_PASSWORD
```

## Pulling the image from GHCR

The GitHub release workflow at `.github/workflows/release.yml` publishes a multi-stage musl static image to `ghcr.io/bxrne/tau` on every published release.

```sh
docker pull ghcr.io/bxrne/tau:latest

# Pin to a release tag for reproducibility
docker pull ghcr.io/bxrne/tau:v0.4.0
```

`docker-compose.yml` pulls `ghcr.io/bxrne/tau:${TAU_IMAGE_TAG:-latest}` by default. Override with `TAU_IMAGE_TAG=v0.4.0` in `.env` to pin.

## Ports (host-visible)

| Port  | Service    | Purpose                                            |
|-------|------------|----------------------------------------------------|
| 7070  | tau        | TauQL query port (TCP, optionally TLS)             |
| 9100  | tau        | Prometheus `/metrics` HTTP endpoint and `/healthz` |
| 9090  | prometheus | Prometheus web UI                                  |
| 3000  | grafana    | Grafana dashboards                                 |

All ports default to bind on `127.0.0.1` only. Override `TAU_BIND_ADDR`, `TAU_METRICS_BIND_ADDR`, `PROM_BIND_ADDR`, `GRAFANA_BIND_ADDR` in `.env` (set to `0.0.0.0` to expose externally; put a reverse proxy in front in that case).

## Environment variables

| Variable              | Required | Default | Notes                                           |
|-----------------------|----------|---------|-------------------------------------------------|
| `TAU_USERNAME`        | yes      | -       | Bootstrap admin username                        |
| `TAU_PASSWORD`        | yes      | -       | Bootstrap admin password (argon2id-hashed)      |
| `TAU_ENCRYPTION_KEY`  | no       | -       | 64-hex string; enables AES-256-GCM at rest      |
| `TAU_IMAGE_TAG`       | no       | latest  | Pinned release tag                              |
| `TAU_LOG_LEVEL`       | no       | info    | error/warn/info/debug/trace                     |
| `TAU_COMPACT_THRESHOLD` | no     | 8       | Layers per lens before auto-compaction          |
| `TAU_CPU_LIMIT`       | no       | 2.0     | docker `deploy.resources.limits.cpus`           |
| `TAU_MEM_LIMIT`       | no       | 512M    | docker `deploy.resources.limits.memory`         |
| `GRAFANA_USER`        | no       | admin   | Grafana login                                   |
| `GRAFANA_PASSWORD`    | yes      | -       | Grafana password (no default)                   |
| `PROM_RETENTION`      | no       | 30d     | Prometheus TSDB retention by time               |
| `PROM_RETENTION_SIZE` | no       | 8GB     | Prometheus TSDB retention by size               |
| `GF_SERVER_ROOT_URL`  | no       | http://localhost:3000 | URL Grafana renders in alert/share links |

The bootstrap admin password is consumed on the very first start when the `tau_data` volume is fresh and `/data/users.json` does not yet exist. From then on, the users file is source of truth - password rotations must happen through `CREATE USER` / `DROP USER` / `GRANT` / `REVOKE` against the running server, not by editing the env var.

## TLS

Tau supports TLS via `--tls --tls-cert <path> --tls-key <path>`. To enable inside the container:

1. Place `server.crt` and `server.key` PEM files in a host directory.
2. Uncomment the volume mount in `docker-compose.yml`:
   ```yaml
   - /etc/tau/tls/server.crt:/data/tls/server.crt:ro
   - /etc/tau/tls/server.key:/data/tls/server.key:ro
   ```
3. Append to the `command:` array in `docker-compose.yml`:
   ```yaml
   - "--tls"
   - "--tls-cert"
   - "/data/tls/server.crt"
   - "--tls-key"
   - "/data/tls/server.key"
   ```
4. Connect with tauctl: `connect prod 127.0.0.1:7070 tls admin <password>`.

If `--tls` is set without explicit cert/key paths the server generates an ephemeral self-signed certificate at boot. Use that only against clients that explicitly accept invalid certs (`tauctl` does, by design - see the README for the rustls verifier note).

## Encryption at rest

Set `TAU_ENCRYPTION_KEY` to a 64-char hex string (32 bytes) to enable AES-256-GCM encryption for WAL entries and Disk-backed lenses:

```sh
# Generate a key
openssl rand -hex 32
```

Store the key in a secrets manager and inject it via `.env` or docker secrets. WAL/Disk files written with a key cannot be read without it; plaintext files remain readable when no key is set.

## Healthcheck

The runtime base image is `scratch` - no shell, no `curl`, no `wget` - so docker's `HEALTHCHECK` directive cannot run inside the container. Use Prometheus instead:

- `up{job="tau"}` indicates scrape reachability; the `TauDown` alert in `prometheus/alerts.yml` fires when it stays 0 for more than a minute.
- Kubernetes/Nomad/etc. probes should call `GET /healthz` on the metrics port directly.

## Metrics endpoints

The tau metrics listener answers:

| Path        | Purpose                                                                                |
|-------------|----------------------------------------------------------------------------------------|
| `/metrics`  | Prometheus text-format counters, histograms, and gauges (see root README for the list) |
| `/healthz`  | Liveness probe; returns 200 with a short text body                                     |
| any other GET | `404 not found`                                                                       |

Every request is logged at `debug` with `peer`, `method`, `path`, `status`, `bytes`, and `elapsed_us`. Raise `TAU_LOG_LEVEL=trace` to see request lines verbatim.

## Prometheus alerts

The provisioned rules in `prometheus/alerts.yml` cover:

| Alert                       | Severity | Trigger                                                  |
|-----------------------------|----------|----------------------------------------------------------|
| `TauDown`                   | critical | scrape fails for 1 minute                                |
| `TauMetricsStale`           | warning  | server up but no statements/connections for 5 minutes    |
| `TauHighErrorRate`          | warning  | error rate > 5 % for 5 minutes                           |
| `TauCriticalErrorRate`      | critical | error rate > 25 % for 2 minutes                          |
| `TauErrorBurst`             | warning  | > 50 errors/s for 1 minute                               |
| `TauAuthFailureSpike`       | warning  | auth failure rate > 20 % for 3 minutes                   |
| `TauAuthBruteForce`         | critical | > 20 failed auth/s for 2 minutes                         |
| `TauConnectionRejections`   | warning  | any connection refused due to the max-connections cap    |
| `TauHighAppendLatency`      | warning  | p95 APPEND latency > 5 ms for 5 minutes                  |
| `TauHighReadLatency`        | warning  | p95 read latency > 2 ms for 5 minutes                    |
| `TauThroughputDrop`         | warning  | throughput < 50 % of 1h average for 10 minutes           |
| `TauHighMemory`             | warning  | resident set size > 768 MiB for 5 minutes                |
| `TauFdPressure`             | warning  | open FDs > 4096 for 5 minutes                            |

Latency alerts use `histogram_quantile` over the per-type `tau_statement_duration_microseconds_bucket` family.

To route alerts somewhere actionable, add an `alertmanager` service and configure `alerting:` in `prometheus.yml`.

## Grafana dashboard

UID `tau-db-prod`. Find it at `http://localhost:3000/d/tau-db-prod`.

Rows:

- **Overview** - status, throughput, connections/s, error rate, auth fail rate, rejected connections.
- **Throughput** - statements/s split by type; accepted vs rejected connections.
- **Latency** - p50/p95/p99 per statement type, plus cumulative bucket-rate distribution.
- **Security** - AUTH attempts vs failures, error rate over time.
- **Resources** - RSS / VSZ / open FDs / threads / uptime, all driven by `tau_process_*` gauges.

## Docker image details

The Dockerfile uses a three-stage build:

- `chef` - rust + musl + cargo-chef (cached until `RUST_VERSION` changes).
- `planner` - derives `recipe.json` from `Cargo.toml`/`Cargo.lock`.
- `builder` - cooks the dependency graph, then compiles `tau` with the configured `BUILD_PROFILE`. Strips and copies the binary.
- `scratch` runtime - zero OS surface. Only `/tau` and the `/data` volume.

```sh
docker build \
  --build-arg RUST_VERSION=1.94.1 \
  --build-arg BUILD_PROFILE=release \
  -f container/Dockerfile \
  -t tau:local .
```

Use `BUILD_PROFILE=release-bench` for fat-LTO builds (slower compile, slightly tighter binary).

## Production hardening checklist

- [ ] Set strong `TAU_PASSWORD` and `GRAFANA_PASSWORD`.
- [ ] Set `TAU_ENCRYPTION_KEY` for encryption at rest.
- [ ] Mount real TLS certificates and enable `--tls`.
- [ ] Put a reverse proxy (nginx/caddy/traefik) in front of Grafana with HTTPS.
- [ ] Configure an Alertmanager target and on-call routing.
- [ ] Tune `TAU_CPU_LIMIT`, `TAU_MEM_LIMIT`, `--max-connections`, `--idle-timeout-secs` for your workload.
- [ ] Back up the `tau_data` volume on a schedule (WAL + users.json).
- [ ] Pin `TAU_IMAGE_TAG` to a release tag rather than `latest` in production.
