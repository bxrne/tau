# storage

Pluggable backing storage for the layer stack. The `Store<V>` trait is the only interface the rest of the library talks to; backends are swapped out at construction time.

## Backends

### `InMemory`

A `HashMap` from lens name to a `Vec<Layer<V>>`. All state lives in process memory and is lost on shutdown. Used by the test suite and as the default backend when no WAL path is given.

The compaction threshold is configurable at construction; the default is 8 layers per lens. Once the stack depth exceeds that threshold, all layers for that lens are merged into a single canonical layer before the new write lands.

### `Disk`

A binary file with a fixed header followed by length-prefixed entries. Each entry records a layer ID, the lens name, and the list of taus. On `open`, the whole file is read and the layer stack is reconstructed in memory; subsequent writes go to memory first, and `flush` rewrites the entire file atomically.

The binary format uses two magic prefixes: `TAU\x01` for plaintext and `TAUE` for encrypted. When a 32-byte key is provided, `flush` encrypts the entire payload with AES-256-GCM before writing. The AEAD authentication tag makes tampering detectable without a separate checksum in the encrypted path; the plaintext path uses CRC32 per-header.

**TODO:** The current flush path is O(total data) because it rewrites the whole file. An append-only mode that writes new entries at the end would make writes O(entry), at the cost of needing a separate compaction pass to reclaim space.

### `Wal`

An append-only flat file where each line is one serialised layer append. The format is human-readable in the unencrypted case:

```
<crc32> <layer_id> <lens_name> <start>:<end>:<value> ...
```

Encrypted entries are prefixed with `E:` and base64-encoded. On startup, `Wal::replay` reads the file top-to-bottom and pushes each entry back into a fresh store — this is how durability is achieved. The WAL itself is the authoritative record; the in-memory state is a derived view of it.

Every call to `Wal::append` issues an `fsync` (via `sync_data`) before returning. This is intentionally synchronous and conservative — it means write latency is bounded below by disk sync latency, but crash recovery guarantees are strong.

**TODO:** The WAL grows without bound. Once a compaction checkpoint is taken, entries that are fully covered by the compacted layer can be truncated from the front of the file. This is the standard log-structured database pattern (see Kleppmann §3, or any LSM-tree implementation).

## Compaction

`compact_layers` is a free function shared by both `InMemory` and `Disk`. It takes the full layer stack and produces a single equivalent layer by:

1. Collecting all tau boundaries across all layers into a sorted, deduplicated list of timestamps
2. For each sub-interval between consecutive boundaries, querying the stack in newest-first order to find the effective value
3. Merging adjacent sub-intervals that have the same value

The result is semantically identical to the original stack under any point lookup — it is a lossless compression. Adjacent merging is important: without it, compaction would explode the tau count for workloads that append many overlapping layers with the same value.

## Design decisions

### Trait object vs. enum dispatch

`Store<V>` is a trait object (`Box<dyn Store<V>>`). An enum dispatch would be faster (no vtable) but would require the `Database` type to be parameterised over the backend variant, which bleeds into public API. The trait object approach keeps `Database<V>` simple. The hot path is compacted down to two layers at most, so the vtable indirection is rarely on the critical path.

### Why `BTreeMap` in `Disk` but `HashMap` in `InMemory`

`Disk` uses `BTreeMap` so that `flush` writes lenses in a deterministic order, making the binary file reproducible and diffable. `InMemory` uses `HashMap` for O(1) amortised access — order doesn't matter there.

### CRC32 vs. cryptographic checksums

CRC32 is used for corruption detection on unencrypted files, not tamper detection. It catches bit-flip errors and partial writes but not adversarial modification. When encryption is enabled, the AEAD tag (GCM authentication) provides tamper detection as a side effect.
