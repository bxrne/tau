+++
title = "Configuration"
date = 2026-05-28
template = "page.html"
+++

Tau is configured entirely through command-line flags and environment variables. There is no config file yet (TOML config is on the roadmap for v0.2.0).

---

## Server (`tau`)

```
Usage: tau [OPTIONS] [ADDR]

Arguments:
  [ADDR]   TCP address to listen on [default: 127.0.0.1:7070]
```

### Storage

| flag | default | description |
|------|---------|-------------|
| `--wal` | off | Enable write-ahead logging for durability |
| `-w, --wal-path <PATH>` | (required with --wal) | Path for the WAL file |
| `--compact-threshold <N>` | 8 | Number of layers per lens before auto-compaction fires |

When `--wal` is enabled, every write is fsynced to the WAL before being applied to the in-memory store. On startup, the WAL is replayed to reconstruct state. Without `--wal`, data is in-memory only and lost on process exit.

### TLS

| flag | default | description |
|------|---------|-------------|
| `--tls` | off | Enable TLS |
| `--tls-cert <PATH>` | (ephemeral self-signed) | PEM certificate file |
| `--tls-key <PATH>` | (ephemeral self-signed) | PEM private key file |

With `--tls` and no cert/key paths, an ephemeral self-signed certificate is generated at startup; convenient for development but not verifiable by clients. For production, provide a real cert and key.

### Authentication

| flag | default | description |
|------|---------|-------------|
| `--auth` | off | Enable per-connection authentication |
| `--username <NAME>` | (none) | Bootstrap admin username (requires `--auth`) |
| `--password <PASS>` | (none) | Bootstrap admin password, hashed with Argon2id at startup |
| `--users-file <PATH>` | (none) | Persistent multi-user store; file is created on first run |

Two user-store modes:

**In-memory single user:** `--auth --username admin --password s3cr3t`. Bootstraps one global-admin user with no persistence. Every restart requires the same flags.

**Persistent multi-user:** `--auth --users-file /var/lib/tau/users`. On first run with `--username`/`--password`, the file is seeded with that user as global admin. Subsequent `CREATE USER`, `DROP USER`, `GRANT`, and `REVOKE` statements are atomically written to the file.

### Observability

| flag | default | description |
|------|---------|-------------|
| `--metrics-port <PORT>` | (none) | Expose Prometheus `/metrics` and `/healthz` on this HTTP port |
| `-l, --log-level <LEVEL>` | `info` | `error` \| `warn` \| `info` \| `debug` \| `trace` |

### Connection management

| flag | default | description |
|------|---------|-------------|
| `--max-connections <N>` | 1024 | Maximum concurrent client connections; beyond the cap, new connections receive `ERR server at connection limit` |
| `--idle-timeout-secs <SECS>` | 300 | Per-connection idle timeout in seconds; 0 disables |

---

## Environment variables

| variable | description |
|----------|-------------|
| `TAU_ENCRYPTION_KEY` | 64 hex characters (32 bytes). When set, WAL entries are encrypted per-entry with AES-256-GCM. Without this key, an encrypted WAL file cannot be replayed. |

Example:

```bash
export TAU_ENCRYPTION_KEY=$(openssl rand -hex 32)
tau --wal -w /var/lib/tau/data.wal
```

---

## Performance flags (server)

These trade durability for throughput. Use only for bulk-load paths or when an external durability boundary (replication, backup) exists.

| flag | default | description |
|------|---------|-------------|
| `--no-fsync-each` | off | Skip per-record WAL flush+sync. A background thread flushes every 50 ms |
| `--no-rewrite-on-compact` | off | Skip disk-file rewrite after compaction |
| `--no-auto-checkpoint` | off | Skip WAL checkpoint rewrite after compaction |

## Client (`ctl`)

`ctl` launches a ratatui TUI by default when stdout is a TTY. Use `--headless` for the line-editor REPL.

| flag | default | description |
|------|---------|-------------|
| `--headless` | off | Use the rustyline REPL instead of the TUI |

## DST (`dst`)

| flag | default | description |
|------|---------|-------------|
| `--tier <TIER>` | `nano` | Workload scale: `nano` (10k rows), `micro` (1M), `small` (100M), `full` (1B) |
| `--seed <N>` | time-based | RNG seed; printed on every run for reproducibility |
| `--no-faults` | off | Disable fault injection |
| `--backend <BACKEND>` | `embedded` | Storage backend: `embedded` (in-memory library), `wal` (disk WAL with replay faults), `tcp` (in-process server over loopback) |
| `--log-level <LEVEL>` | `info` | `tracing` log level |

---

## Metrics reference

When `--metrics-port` is set, the server exposes:

```
GET http://127.0.0.1:<PORT>/metrics    Prometheus text-format
GET http://127.0.0.1:<PORT>/healthz    Liveness probe (returns "ok")
```

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

When `--auth` is enabled, every statement is checked against the caller's CRUDA bitmap:

| bit | grants |
|-----|--------|
| `C` | `CREATE LENS`, `DERIVE LENS` |
| `R` | `AT`, `RANGE`, `REDUCE`, `SHOW LENSES` |
| `U` | `APPEND LENS`, `COPY LENS FROM` |
| `D` | `DROP LENS` |
| `A` | Admin: manage users, `GRANT`/`REVOKE`, `CREATE DATABASE`, `DROP DATABASE` |

Effective permissions for a user on database `db` = `grants[db] | grants["*"]`. A user with `A` on `"*"` is a global admin.

`SHOW DATABASES` is post-filtered for non-admins: only databases the caller holds any grant on are returned.
