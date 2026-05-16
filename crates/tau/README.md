# tau server

The TCP server binary. Exposes a `libtau` executor over a line-oriented TCP protocol.

## Configuration

The server reads `config.toml` in the current working directory, or a path supplied with `--config`:

```bash
tau                          # uses ./config.toml if present, otherwise defaults
tau --config /etc/tau/cfg.toml
```

A sample config lives at the repo root. Key sections:

```toml
bind = "127.0.0.1:7070"
log_level = "info"
compact_threshold = 8

[wal]
enabled = true
path = "/var/lib/tau/tau.wal"
max_size_mb = 512           # rotate WAL when it reaches 512 MiB

[tls]
enabled = true
cert = "/etc/tau/cert.pem"
key  = "/etc/tau/key.pem"

[auth]
enabled = true
username = "admin"
password = "changeme"
users_file = "/var/lib/tau/users.db"

[metrics]
port = 9100

[limits]
max_connections = 1024
idle_timeout_secs = 300
```

**WAL encryption:** Set `TAU_ENCRYPTION_KEY` (64 hex chars = 32 bytes) to enable per-entry
AES-256-GCM encryption. Generate a key with `openssl rand -hex 32`. The key is never stored
on disk; without it an encrypted WAL cannot be replayed.

```bash
export TAU_ENCRYPTION_KEY=$(openssl rand -hex 32)
cargo run --release --bin tau -- --config /etc/tau/config.toml
```

See [docs/configuration](https://tau.bxrne.com/docs/configuration/) for the full reference.

## Wire protocol

One statement per line in, one response line out. The wire codec lives in `libtau::wire::Response` — both the server encoder and the client decoder use the same type.

| Response | Meaning |
|----------|---------|
| `OK` | DDL or write succeeded |
| `OK BYE` | Server acknowledged `QUIT` / `EXIT` |
| `VAL <codec>` | Point lookup returned a value |
| `VAL NIL` | Point lookup: no tau covers that timestamp |
| `RANGE <n>; <s>:<e>:<v>; ...` | Range scan returned `n` segments |
| `NAMES <n>; name; ...` | Name list from `SHOW DATABASES` / `SHOW LENSES` / `SHOW USERS` |
| `GRANTS <n>; <user> <db>:<perms>; ...` | Output of `SHOW GRANTS` |
| `LAYERS <n>; <id>:<written_at>:<min>:<max>; ...` | Output of `HISTORY LENS` |
| `ERR <message>` | Parse failure, executor error, or permission denial |

Values are encoded with a one-character type tag: `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

## Authentication

When `[auth] enabled = true`, the first message from every client must be `AUTH <username> <password>`. After a successful `AUTH`, the session is bound to that user and every subsequent statement dispatches through `exec_as` for CRUDA grant enforcement.

## Concurrency model

Per-connection OS threads, one `Arc<RwLock<Executor>>` wrapping a `FxHashMap<String, Arc<RwLock<DbState>>>`.

Lock routing in `handle_query`:
- Read-only statements: shared executor lock + per-DB read lock inside `exec_read`.
- Registry writes (`CREATE DATABASE`, user management, transactions): exclusive executor lock.
- Data writes (`APPEND`, `CREATE LENS`, etc.): shared executor lock + per-DB write lock inside `exec_db_write`.

## Running

```bash
cargo run --release --bin tau                       # in-memory, defaults
cargo run --release --bin tau -- --config cfg.toml  # with config file
```
