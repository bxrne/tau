+++
title = "Configuration"
date = 2026-05-28
template = "page.html"
+++

Tau is configured via a TOML file. The server looks for `config.toml` in the
current working directory unless `--config <path>` is passed. All fields are
optional; an absent config file starts an in-memory server on `127.0.0.1:7070`
with defaults.

---

## Quick start

```bash
# Copy the sample and edit
cp config.toml my-tau.toml

# Start with the default config.toml in the current directory
tau

# Or point at a specific path
tau --config /etc/tau/config.toml
```

---

## Full example

```toml
bind = "127.0.0.1:7070"
log_level = "info"         # error | warn | info | debug | trace
compact_threshold = 8      # layers per lens before auto-compaction fires

[disk]
compression_level = 3      # zstd level 1–22; higher = better ratio, slower writes

[wal]
enabled = true
path = "/var/lib/tau/tau.wal"
no_fsync_each = false       # true: 50 ms group-commit; risk: up to one interval of data loss
no_rewrite_on_compact = false
no_auto_checkpoint = false

[tls]
enabled = true
cert = "/etc/tau/cert.pem"  # omit both cert+key for ephemeral self-signed (dev only)
key  = "/etc/tau/key.pem"

[auth]
enabled = true
username = "admin"          # bootstrap admin on first run
password = "changeme"       # hashed with argon2id at startup; plaintext not retained
users_file = "/var/lib/tau/users.db"

[metrics]
port = 9100                 # serves GET /metrics and GET /healthz

[limits]
max_connections = 1024
idle_timeout_secs = 300     # 0 disables
```

---

## Top-level fields

| field | default | description |
|-------|---------|-------------|
| `bind` | `127.0.0.1:7070` | TCP address to listen on |
| `log_level` | `info` | `error` \| `warn` \| `info` \| `debug` \| `trace` |
| `compact_threshold` | `8` | Number of layers per lens before automatic compaction fires |

---

## `[disk]`

| field | default | description |
|-------|---------|-------------|
| `compression_level` | `3` | zstd compression level applied when the disk store flushes. Range 1–22: 1 is fastest with least compression, 22 is best ratio but slowest. |

---

## `[wal]`

| field | default | description |
|-------|---------|-------------|
| `enabled` | `false` | Enable write-ahead logging for durability across restarts |
| `path` | (none) | Path for the WAL file — required when `enabled = true` |
| `no_fsync_each` | `false` | Skip per-record WAL flush+sync; a background thread flushes every 50 ms |
| `no_rewrite_on_compact` | `false` | Skip disk-file rewrite after compaction |
| `no_auto_checkpoint` | `false` | Skip WAL checkpoint rewrite after compaction |

When `enabled = true`, every write is fsynced to the WAL before being applied
to the in-memory store. On startup, the WAL is replayed to reconstruct state.
Without the WAL, data is in-memory only and lost on process exit.

The `no_*` fields trade durability for throughput. Use only on trusted
workloads or when an external durability boundary (replication, backup) exists.

---

## `[tls]`

| field | default | description |
|-------|---------|-------------|
| `enabled` | `false` | Enable TLS |
| `cert` | (none) | Path to PEM-encoded certificate file |
| `key` | (none) | Path to PEM-encoded private key file |

With `enabled = true` and no `cert`/`key` paths, an ephemeral self-signed
certificate is generated at startup — convenient for development but not
verifiable by clients. For production, provide a real cert and key.

---

## `[auth]`

| field | default | description |
|-------|---------|-------------|
| `enabled` | `false` | Enable per-connection authentication |
| `username` | (none) | Bootstrap admin username |
| `password` | (none) | Bootstrap admin password, hashed with Argon2id at startup |
| `users_file` | (none) | Persistent multi-user store; created on first run |

Two user-store modes:

**In-memory single user** — set `username`/`password`, no `users_file`. Bootstraps
one global-admin user with no persistence. Every restart requires the same values.

**Persistent multi-user** — set `users_file`. On first run with `username`/`password`,
the file is seeded with that user as global admin. Subsequent `CREATE USER`,
`DROP USER`, `GRANT`, and `REVOKE` statements are atomically written back.

---

## `[metrics]`

| field | default | description |
|-------|---------|-------------|
| `port` | (none) | Expose Prometheus `/metrics` and `/healthz` on this HTTP port |

When `port` is set:

```
GET http://0.0.0.0:<port>/metrics    Prometheus text-format
GET http://0.0.0.0:<port>/healthz    Liveness probe
```

---

## `[limits]`

| field | default | description |
|-------|---------|-------------|
| `max_connections` | `1024` | Maximum concurrent client connections; new connections beyond the cap receive `ERR server at connection limit` |
| `idle_timeout_secs` | `300` | Per-connection idle timeout in seconds; `0` disables |

---

## Environment variables

| variable | description |
|----------|-------------|
| `TAU_ENCRYPTION_KEY` | 64 hex characters (32 bytes). When set, WAL entries are encrypted per-entry with AES-256-GCM. Without this key, an encrypted WAL file cannot be replayed. |

```bash
export TAU_ENCRYPTION_KEY=$(openssl rand -hex 32)
tau --config /etc/tau/config.toml
```

---

## Metrics reference

| metric | type | description |
|--------|------|-------------|
| `tau_statements_total{type=...}` | counter | Statements processed, by type |
| `tau_statement_duration_microseconds_bucket{type=...,le=...}` | histogram | Per-type latency histogram |
| `tau_connections_total` | counter | TCP connections accepted since startup |
| `tau_rejected_connections_total` | counter | Connections refused at the max-connections cap |
| `tau_auth_attempts_total` | counter | `AUTH` messages received |
| `tau_auth_failures_total` | counter | Failed `AUTH` attempts |
| `tau_errors_total` | counter | `ERR` responses sent |
| `tau_process_resident_bytes` | gauge | Resident memory (Linux: VmRSS) |
| `tau_process_open_fds` | gauge | Open file descriptors |
| `tau_process_uptime_seconds` | gauge | Seconds since startup |

---

## Permission model

When `[auth] enabled = true`, every statement is checked against the caller's CRUDA bitmap:

| bit | grants |
|-----|--------|
| `C` | `CREATE LENS`, `DERIVE LENS` |
| `R` | `AT`, `RANGE`, `REDUCE`, `SHOW LENSES` |
| `U` | `APPEND LENS`, `COPY LENS FROM` |
| `D` | `DROP LENS` |
| `A` | Admin: manage users, `GRANT`/`REVOKE`, `CREATE DATABASE`, `DROP DATABASE` |

Effective permissions for a user on database `db` = `grants[db] | grants["*"]`.
A user with `A` on `"*"` is a global admin.

`SHOW DATABASES` is post-filtered for non-admins: only databases the caller
holds any grant on are returned.

---

## Client (`ctl`)

`ctl` has no configuration file. It accepts only `--version` and `--help`;
all connection and session settings are entered interactively in the TUI.

