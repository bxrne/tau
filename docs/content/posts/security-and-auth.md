+++
title = "Security and auth"
date = 2026-05-23
description = "TLS, authentication, and encryption at rest."
tags = ["security", "auth", "tls"]
categories = ["operations"]
+++

Tau has three opt-in security layers: TLS for transport, authentication with
per-database CRUDA grants, and AES-256-GCM encryption at rest.

## TLS

```bash
cargo run --release -- --tls
cargo run --release -- --tls --tls-cert server.crt --tls-key server.key
```

## Authentication and grants

```tauql
CREATE USER alice PASSWORD "p4ss";
GRANT R ON main TO alice;
GRANT A ON * TO admin;
```

Permissions are applied per database, plus a wildcard `*` that covers every
current and future database.

## Encryption at rest

Set `TAU_ENCRYPTION_KEY` to a 64-character hex string to enable WAL encryption:

```bash
TAU_ENCRYPTION_KEY=$(openssl rand -hex 32) \
  cargo run --release -- --wal -w data.wal
```
