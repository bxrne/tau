# tau server

## What it is

The TCP server binary. It exposes a `libtau` kernel over a line-oriented protocol — one TauQL statement per line in, one response line out — with optional TLS, authentication, and durable storage. All engine logic lives in `libtau`; this crate is transport, configuration, and metrics.

## How it works

The wire codec is `libtau::wire::Response`, shared by the server encoder and client decoder. Responses are `OK`, `OK BYE` (after `QUIT`/`EXIT`), `VAL <codec>` / `VAL NIL`, `RANGE <n>; <s>:<e>:<v>; …`, `NAMES …`, `GRANTS …`, `LAYERS …` (from `HISTORY`), or `ERR <message>`. Values carry a one-character type tag: `i<int>`, `f<float>`, `s<percent-escaped>`, `b<0|1>`, `n` (null).

When `[auth] enabled = true`, the first client message must be `AUTH <username> <password>`; the session is then bound to that user and every statement dispatches through `exec_as` for CRUDA grant enforcement.

Each connection runs on its own OS thread; all threads share one plain `Arc<Kernel>` — the server has no lock router. The kernel routes each statement to the owning service and locks internally: readers never block each other, a data write locks only its database, and database DDL takes the registry write lock briefly. `tau::harness::EphemeralServer` spawns this server on `127.0.0.1:0` with an in-memory or supplied `Kernel` for the DST wire profiles, so the accept loop and auth handshake are exercised by simulation testing.

## Using it

```bash
# Release binary (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tau-x86_64-linux -o tau
chmod +x tau && sudo mv tau /usr/local/bin/

cargo install --git https://github.com/bxrne/tau tau          # from source
docker run -p 7070:7070 ghcr.io/bxrne/tau:latest              # Docker

tau                              # in-memory, ./config.toml if present
tau --config /etc/tau/cfg.toml   # explicit config
```

Configuration is TOML: `bind`, `log_level`, `compact_threshold`, and the `[disk]` (memory or SSTable backend + zstd level), `[wal]` (path, `max_size_mb`, fsync mode), `[tls]`, `[auth]` (bootstrap user + `users_file`), `[metrics]` (Prometheus port), and `[limits]` (`max_connections`, `idle_timeout_secs`) sections — full reference at [docs/configuration](https://tau.bxrne.com/docs/configuration/). Setting `TAU_ENCRYPTION_KEY` (64 hex chars, e.g. `openssl rand -hex 32`) enables AES-256-GCM encryption of WAL and disk files; the key is never stored on disk.
