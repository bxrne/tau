+++
title = "Running the server"
date = 2026-05-25
description = "Starting tau locally, enabling auth, and checking metrics."
tags = ["server", "operations"]
categories = ["guides"]
+++

Run the server with:

```bash
cargo run --release
```

The server listens on `127.0.0.1:7070` by default. Each connection is handled by
its own OS thread; reads share the executor lock, while writes take it
exclusively.

## Auth and TLS

```bash
cargo run --release -- --auth --users-file /tmp/tau.users --username admin --password s3cret
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

With `--auth`, the first message from every client must be `AUTH <user> <pass>`.
A user with `A` on `*` is a global admin.

## Metrics

When started with `--metrics-port <PORT>`, the server exposes Prometheus metrics
at `GET /metrics` and liveness at `GET /healthz`.
