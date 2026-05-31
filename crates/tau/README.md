# tau server

The TCP server binary. Exposes a `libtau` executor over a line-oriented TCP protocol.

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

When `--auth` is set, the first message from every client must be `AUTH <username> <password>`. After a successful `AUTH`, the session is bound to that user and every subsequent statement dispatches through `exec_as` for CRUDA grant enforcement.

Two user-store modes:

- **In-memory single user**: `--auth --username admin --password s3cr3t`. Bootstraps one global-admin user; no persistence.
- **Persistent multi-user**: `--auth --users-file /path/users.json`. Loads and persists all user mutations.

## Concurrency model

Per-connection OS threads, one `Arc<RwLock<Executor>>` wrapping a `FxHashMap<String, Arc<RwLock<DbState>>>`.

Lock routing in `handle_query`:
- Read-only statements: shared executor lock + per-DB read lock inside `exec_read`.
- Registry writes (CREATE DATABASE, user management, transactions): exclusive executor lock.
- Data writes (APPEND, CREATE LENS, etc.): shared executor lock + per-DB write lock inside `exec_db_write`. Reads to database A proceed concurrently with writes to database B.

## Performance flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--no-fsync-each` | off | Skip per-record WAL sync; a background thread flushes every 50 ms |
| `--no-rewrite-on-compact` | off | Skip disk-file rewrite after compaction |
| `--no-auto-checkpoint` | off | Skip WAL checkpoint rewrite after compaction |

## Running

```bash
cargo run --release --bin tau                              # in-memory, 127.0.0.1:7070
cargo run --release --bin tau -- --wal -w /tmp/tau.wal    # with WAL
cargo run --release --bin tau -- --tls                    # ephemeral self-signed TLS
cargo run --release --bin tau -- --auth --username u --password p
cargo run --release --bin tau -- --no-fsync-each --wal -w /tmp/tau.wal
```
