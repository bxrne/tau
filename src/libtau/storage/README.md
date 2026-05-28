# storage

Pluggable backing storage for the layer stack. The `Store<V>` trait is the only interface the rest of the library talks to; backends are swapped out at construction time.

## Backends

### `InMemory`

A `HashMap` from lens name to a `Vec<Layer<V>>`. All state lives in process memory and is lost on shutdown. Used by the test suite and as the default backend when no WAL path is given.

The compaction threshold is configurable at construction; the default is 8 layers per lens. Once the stack depth exceeds that threshold, all layers for that lens are merged into a single canonical layer before the new write lands.

### `Disk`

A binary file with a fixed header followed by length-prefixed entries. Each entry records a layer ID, the lens name, and the list of taus. On `open`, the whole file is read and the layer stack is reconstructed in memory; subsequent writes go to memory first, and `flush` rewrites the entire file atomically.

The binary format uses two magic prefixes: `TAU\x01` for plaintext and `TAUE` for encrypted. When a 32-byte key is provided, `flush` encrypts the entire payload with AES-256-GCM before writing. The AEAD authentication tag makes tampering detectable without a separate checksum in the encrypted path; the plaintext path uses CRC32 per-header.

Unencrypted `Disk` stores hold an open append-mode file handle; writes are O(entry). Encrypted stores still rewrite on flush - the AEAD tag covers the entire payload, so partial appends aren't possible. A compaction triggers a full rewrite in both cases.

### `Wal`

An append-only flat text file. Each line is one of three entry types:

- Data entry: `<crc32hex> <base64-payload>` - a binary-encoded layer, base64-wrapped with a CRC32 integrity check.
- Schema entry: `S:<crc32hex> <stmt_text>` - a raw `CREATE LENS` or `DERIVE LENS` or `DROP LENS` statement, replayed to reconstruct the schema on startup.
- Encrypted schema entry: `SE:<crc32hex> <base64-encrypted-DDL>` - AES-256-GCM encrypted schema line.

On startup, `Wal::replay` reads data entries top-to-bottom and pushes each layer back into a fresh store. Schema lines are returned separately and replayed through the executor after data replay. The WAL is the authoritative durability record; the in-memory state is a derived view of it.

Every call to `Wal::append` issues an `fsync` (via `sync_data`) before returning. This is intentionally synchronous and conservative - write latency is bounded below by disk sync latency, but crash recovery guarantees are strong.

After auto-compaction fires, `Database::checkpoint` rewrites the WAL atomically (write to `.tmp`, fsync, rename) to contain only the live post-compaction layers plus any schema lines. WAL growth is bounded to the current live data set.

## Compaction

`compact_layers` is a free function shared by both `InMemory` and `Disk`. It takes the full layer stack and produces a single equivalent layer by:

- Collecting all tau boundaries across all layers into a sorted, deduplicated list of timestamps.
- For each sub-interval between consecutive boundaries, querying the stack in newest-first order to find the effective value.
- Merging adjacent sub-intervals that share a value.

The result is semantically identical to the original stack under any point lookup - it is a lossless compression. Adjacent merging is important: without it, compaction would explode the tau count for workloads that append many overlapping layers with the same value.

## Design decisions

### Trait object vs. enum dispatch

`Store<V>` is a trait object (`Box<dyn Store<V>>`). An enum dispatch would be faster (no vtable) but would require the `Database` type to be parameterised over the backend variant, which bleeds into public API. The trait object approach keeps `Database<V>` simple. The hot path is compacted down to two layers at most, so the vtable indirection is rarely on the critical path.

### `HashMap` in both backends

Both `Disk` and `InMemory` use `HashMap<String, Vec<Layer<V>>>` for O(1) amortised lens lookup. `Disk::flush` rewrites the whole file from the in-memory state, so write order is whatever the HashMap iterator produces; the file format is self-describing (length-prefixed lens names + per-entry checksums) and does not need a stable on-disk ordering.

### CRC32 vs. cryptographic checksums

CRC32 is used for corruption detection on unencrypted files, not tamper detection. It catches bit-flip errors and partial writes but not adversarial modification. When encryption is enabled, the AEAD tag (GCM authentication) provides tamper detection as a side effect.
