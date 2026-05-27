+++
title = "Tau docs"
description = "Notes and guides for working with the Tau time series engine."
paginate_by = 5
sort_by = "date"
+++

Tau is a time-series database built on immutable, layered temporal intervals.
Corrections are modeled by appending new layers on top of existing data, and the
newest layer wins at query time. That means you keep the full correction history
without write-write conflicts.

## Quick start

```bash
cargo run --release                           # in-memory, listens on 127.0.0.1:7070
cargo run --release -- --wal -w data.wal     # with WAL durability
```

```tauql
CREATE DATABASE main;
CREATE LENS temp float;
APPEND LENS temp 0 50 18.5, 50 100 21.0;
AT LENS temp 25;
```

## Core concepts

- **Tau<V>** - an immutable value over a half-open interval `[start, end)`.
- **Layer<V>** - a sorted, non-overlapping batch of taus; newest layer wins.
- **Lens<V>** - a named handle; either `Base` (storage-backed) or `Derived`.

Start with the TauQL basics guide, then review the server and storage posts for
operational details. Release notes live in the changelog section.
