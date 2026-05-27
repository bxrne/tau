+++
title = "Tauctl REPL"
date = 2026-05-24
description = "Working with tauctl for interactive sessions and CSV loads."
tags = ["tauctl", "client"]
categories = ["guides"]
+++

`tauctl` is the interactive REPL for Tau. It reads one line at a time and sends
unknown commands to the active connection as tauql statements.

```text
τ: connect dev 127.0.0.1:7070
τ: CREATE DATABASE demo
τ: AT LENS temp 25
```

## Built-in commands

- `connect <name> <host:port> [tls] [<user> <pass>]`
- `disconnect <name>`
- `use <name>`
- `connections`
- `auth <user> <pass>`
- `load <lens> <local-path> [chunk]`
- `help`, `clear`, `exit`

## Client-side CSV loads

Use `load` when the file lives on your machine and the server is remote. The
command reads the CSV locally and ships batched `APPEND` statements over the
active connection.
