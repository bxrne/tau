+++
title = "Storage and compaction"
date = 2026-05-26
description = "How layers, WAL, and compaction work together in the Tau store."
tags = ["storage", "wal", "compaction"]
categories = ["architecture"]
+++

Tau stores data in immutable layers. Each append writes a layer and records the
operation in the WAL before committing to the store. Compaction flattens layers
into a canonical representation once per-lens thresholds are reached.

## Backends

- **InMemory**: HashMap-backed, volatile on shutdown.
- **Disk**: Binary file with a `TAU` or `TAUE` magic header; encrypted when a key
  is provided.
- **Wal**: Append-only durability log with CRC32 (plain) or AES-256-GCM entries.

## Compaction

The compactor collects all boundaries across layers, evaluates the newest value
per interval, and merges adjacent segments with the same value. The result is
semantically identical but keeps point lookups at O(log n).

If you are tuning ingest throughput, consider the "fast" settings used by the
bench binary. For production workloads, keep the defaults to preserve fsync
behavior.
