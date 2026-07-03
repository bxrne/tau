# Tau DB container stack

## What it is

The production Docker stack: Tau DB plus Prometheus and Grafana. The runtime base image is `scratch` — no shell, no curl — so health checking goes through the metrics endpoint (`up{job="tau"}` in Prometheus, or `GET /healthz` on the metrics port for Kubernetes/Nomad probes). Bundled Prometheus rules alert on server down, high error rate, auth brute-force, latency, memory, and connection rejections; the Grafana dashboard (`tau-db-prod` at `http://localhost:3000/d/tau-db-prod`) has Overview, Throughput, Latency, Security, and Resources rows.

## How it works

All server settings live in `tau-config.toml`, mounted as `/data/tau-config.toml` — bind, WAL, auth, metrics port, connection limits. Only secrets and infrastructure overrides belong in `.env` (copied from `.env.example`): `TAU_ENCRYPTION_KEY` (64 hex chars, enables AES-256-GCM at rest — files written with it cannot be read without it), `GRAFANA_PASSWORD`, and `TAU_IMAGE_TAG` (pin to a release for reproducibility; compose uses `ghcr.io/bxrne/tau:${TAU_IMAGE_TAG:-latest}`).

Host ports: 7070 (TauQL, optionally TLS), 9100 (Prometheus metrics + `/healthz`), 9090 (Prometheus UI), 3000 (Grafana). All default to `127.0.0.1`; override the `*_BIND_ADDR` variables in `.env` to expose externally. For TLS, enable `[tls]` in `tau-config.toml` with cert/key paths, uncomment the TLS volume mounts in `docker-compose.yml`, and connect with `connect prod 127.0.0.1:7070 tls`; with no cert/key set the server generates an ephemeral self-signed cert for development.

## Using it

```sh
cd container
cp .env.example .env          # fill in secrets
$EDITOR tau-config.toml       # set [auth] username/password
docker compose up -d

tauctl                        # τ connect prod 127.0.0.1:7070 → AUTH admin <password>
open http://localhost:3000    # Grafana (GRAFANA_USER / GRAFANA_PASSWORD)
```

Production hardening: strong `[auth]` and Grafana passwords, `TAU_ENCRYPTION_KEY` set, real TLS certificates, a reverse proxy with HTTPS in front of Grafana, Alertmanager routing, scheduled backups of the `tau_data` volume, and a pinned `TAU_IMAGE_TAG`.
