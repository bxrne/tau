+++
title = "TauQL Language Reference"
date = 2026-05-28
template = "page.html"
+++

TauQL is a line-oriented command language. **One statement in, one response line out.** Keywords are case-insensitive. Identifiers are case-sensitive.

## Databases

### `CREATE DATABASE <name>`

Creates a new named database. The first created database becomes the active database automatically.

```
CREATE DATABASE sensors
→ OK
```

Requires global admin (`A on *`) when auth is enabled.

### `DROP DATABASE <name>`

Drops a database and all its lenses permanently.

```
DROP DATABASE sensors
→ OK
```

Requires `A` permission on the named database.

### `SHOW DATABASES`

Lists all database names the caller has access to.

```
SHOW DATABASES
→ NAMES 2; metrics; sensors
```

Global admins see all databases. Non-admin users see only databases they hold at least one grant on.

### `SHOW STATUS`

Returns live server metrics as key-value pairs.

```
SHOW STATUS
→ STATUS 4; uptime_secs:312; databases:2; lenses:7; wal_bytes:4096
```

| key | value |
|-----|-------|
| `uptime_secs` | seconds since the executor was created |
| `databases` | number of named databases |
| `lenses` | total base + derived lens count across all databases |
| `wal_bytes` | total WAL file size on disk (0 for in-memory backends) |

No permission required.

---

## Lenses

### `CREATE LENS <name> <type>`

Creates a base lens in the active database with the given value type.

```
CREATE LENS temperature float
→ OK
```

Types:

| type | Rust equivalent | Wire encoding |
|------|-----------------|---------------|
| `int` | `i64` | `i<value>` |
| `float` | `f64` | `f<value>` |
| `str` | `String` | `s<percent-encoded>` |
| `bool` | `bool` | `b0` or `b1` |
| `bytes` | `Vec<u8>` | `n` (null-encoded; raw bytes not transmitted) |

Requires `C` permission on the active database.

### `DROP LENS <name>`

Removes a lens and all its data from the active database.

```
DROP LENS temperature
→ OK
```

Requires `D` permission on the active database.

### `SHOW LENSES`

Lists all lens names in the active database, sorted alphabetically.

```
SHOW LENSES
→ NAMES 3; cpu; pressure; requests
```

Requires `R` permission.

### `DERIVE LENS <name> AS <expr> [OVER <start> <end>]`

Creates a derived lens. The expression is compiled into a lazy closure; nothing is materialised. Every query re-evaluates the expression at the requested timestamp.

```
DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0
→ OK

DERIVE LENS cpu_smooth AS avg(cpu, -600, 0)
→ OK

DERIVE LENS cpu_hot AS cpu > avg(cpu, -1800, 0)
→ OK
```

The optional `OVER <start> <end>` clause bounds the lens's domain to the half-open interval `[start, end)`. Outside that window the lens reads as `NIL`, even where its source lenses are covered.

```
DERIVE LENS daytime AS sensor OVER 0 43200
→ OK
```

Cycle detection runs at `DERIVE` time. If the expression would create a dependency cycle, the statement is rejected with `ERR cycle detected`.

Derived lenses work with all read statements: `AT`, `RANGE`, and `REDUCE`.

Requires `C` permission on the active database.

### `XDERIVE LENS <name> AS <expr> [OVER <start> <end>]`

Creates a **materialised** lens. Unlike `DERIVE`, the expression is evaluated immediately and the result is stored as concrete layers — so reads are as fast as a base lens and the lens has its own layer history (`HISTORY`, `AT ... LAYER`, `AT ... AS OF`).

The view stays current automatically: whenever a lens it references is written (for example, a correction), the affected segments are recomputed and appended as a new layer, so the newest result wins. You never re-run `XDERIVE` to refresh it.

```
CREATE LENS c int
APPEND LENS c 0 100 10
XDERIVE LENS doubled AS c * 2
AT LENS doubled 50
→ VAL i20

APPEND LENS c 0 100 7          # a correction to the source
AT LENS doubled 50
→ VAL i14                       # auto-updated, no re-run
```

Without an `OVER` clause the materialisation domain is taken from the referenced lenses' extents; with `OVER <start> <end>` it is fixed to `[start, end)`.

Because the lens is engine-maintained, `APPEND`/`COPY` directly into it is rejected with `ERR cannot write to materialised lens` — write the source lenses instead. Cycle detection runs at `XDERIVE` time. The definition and its data survive restarts.

Requires `C` permission on the active database.

### `SET TTL LENS <name> <secs>`

Configures a time-to-live policy for a lens. After this, queries hide data whose temporal interval ends more than `secs` seconds before the current query time. The policy survives restarts (persisted in the schema WAL).

```
SET TTL LENS sensor 86400
→ OK
```

With a 86400-second TTL, a query at Unix time 1_000_000 will not return intervals that end before 1_000_000 - 86400 = 913_600.

- `AT` returns `VAL NIL` for any timestamp before the cutoff.
- `RANGE` omits segments that fall entirely before the cutoff; the effective query start is `max(requested_start, cutoff)`.
- `REDUCE` computes the aggregate over `[max(start, cutoff), end)`.

Requires `C` permission on the active database.

### `UNSET TTL LENS <name>`

Removes the TTL policy for a lens, making all previously hidden data visible again.

```
UNSET TTL LENS sensor
→ OK
```

Requires `C` permission on the active database.

---

## Writes

### `APPEND LENS <name> <s> <e> <v> [, <s> <e> <v> ...]`

Appends one or more temporal intervals to a lens. Multiple intervals in a single `APPEND` form one atomic layer: they either all succeed or all fail.

```
APPEND LENS temperature 0 3600 18.5
→ OK

APPEND LENS cpu 0 60 45, 60 120 72, 120 180 68
→ OK
```

Interval semantics: `s` (start, inclusive), `e` (end, exclusive), `v` (value). Intervals in a single batch may arrive in any order — the server sorts them — but they must not overlap and each must satisfy `s < e`; a violation rejects the whole statement with `ERR invalid range (start >= end)` and nothing is written.

Requires `U` permission on the active database.

### `BATCH APPEND LENS <name> { <s> <e> <v> [; <s> <e> <v> ...] }`

Block-syntax bulk ingest. All intervals inside the braces form one atomic layer. Taus are separated by `;`; a trailing semicolon before `}` is allowed. An empty block `{}` succeeds and writes no data.

```
BATCH APPEND LENS cpu { 0 60 45 ; 60 120 72 ; 120 180 68 }
→ OK
```

Semantically equivalent to a single `APPEND` with the same intervals. Prefer `BATCH APPEND` when building statements programmatically — the block syntax is easier to generate than comma-separated inline lists.

Requires `U` permission on the active database.

### `COPY LENS <name> FROM "<path>"`

Server-side CSV ingest. Reads a file from the server's own filesystem. The file must follow the `start,end,value` format per line; `#` comments and blank lines are ignored.

```
COPY LENS cpu FROM "/data/cpu-load.csv"
→ OK
```

Use `COPY` when the CSV file lives on the server or in a Docker volume. Use the `tauctl` `load` command for client-side ingestion when the file is on your local machine.

Requires `U` permission on the active database.

---

## Transactions

Transactions provide multi-statement atomicity across one or more databases. `USE DATABASE` executes immediately (it changes the active database context for subsequent statements), so a single transaction can buffer writes to multiple databases.

### `START TRANSACTION`

Begins a transaction on the current connection. Subsequent mutations are buffered in memory and not applied to storage until `COMMIT`.

```
START TRANSACTION
→ OK
```

Issuing `START TRANSACTION` while a transaction is already active returns `ERR transaction already active`.

### `COMMIT`

Applies all buffered mutations atomically. Each statement is replayed against the database that was active when it was buffered. Every other connection sees either none of the batch or all of it — there is no partial visibility.

```
START TRANSACTION
APPEND LENS cpu 0 3600 45
APPEND LENS cpu 3600 7200 60
COMMIT
→ OK
```

Multi-database example — writes to two databases atomically:

```
START TRANSACTION
USE DATABASE metrics
APPEND LENS requests 0 60 1042
USE DATABASE audit
APPEND LENS events 0 60 "request_batch"
COMMIT
→ OK
```

Issuing `COMMIT` with no active transaction returns `ERR no active transaction`.

### `ROLLBACK`

Discards all buffered mutations without applying them.

```
START TRANSACTION
APPEND LENS cpu 0 3600 45
ROLLBACK
→ OK
```

`AT LENS cpu 1800` after the rollback above returns `VAL NIL` — the append was discarded.

Issuing `ROLLBACK` with no active transaction returns `ERR no active transaction`.

---

## Reads

### `AT LENS <name> <timestamp>`

Point lookup. Returns the value of the lens at exactly timestamp `t`. If no interval covers `t`, returns `VAL NIL`.

```
AT LENS temperature 1800
→ VAL f18.5

AT LENS temperature 99999
→ VAL NIL
```

The newest-layer-wins rule applies: if multiple layers have an interval covering `t`, the one with the highest layer ID (most recently appended) is returned.

Requires `R` permission.

### `AT LENS <name> <timestamp> AS OF <written_at>`

Temporal point query scoped to layers that were written at or before `written_at` (milliseconds since Unix epoch). Useful for auditing what a lens looked like at a specific wall-clock time.

```
AT LENS temperature 1800 AS OF 1717000000000
→ VAL f17.2
```

Only works on base lenses; derived lenses return an error.

Requires `R` permission.

### `AT LENS <name> <timestamp> LAYER <layer_id>`

Point lookup restricted to a single layer identified by its integer ID. Returns `VAL NIL` if the specified layer does not exist or does not cover `t`. Use `HISTORY LENS` to discover layer IDs.

```
HISTORY LENS temperature
→ LAYERS 3; 1:0:0:3600; 2:0:3600:7200; 3:0:7200:10800

AT LENS temperature 1800 LAYER 1
→ VAL f18.5
```

Only works on base lenses; derived lenses return an error.

Requires `R` permission.

### `RANGE LENS <name> <start> <end> [WHERE <expr>] [LIMIT <n> [OFFSET <m>]]`

Range scan over the interval `[start, end)`. Returns all temporal segments within that window, optionally filtered, limited, or paged.

```
RANGE LENS cpu 0 3600
→ RANGE 60; 0:60:i45; 60:120:i72; ...

RANGE LENS cpu 0 86400 WHERE cpu > 70
→ RANGE 12; 44340:44400:i75; ...

RANGE LENS cpu 0 86400 LIMIT 10
→ RANGE 10; 0:60:i45; ...

RANGE LENS cpu 0 86400 LIMIT 10 OFFSET 20
→ RANGE 10; ...  (segments 21–30)
```

Adjacent segments with the same value are merged automatically. The `WHERE` clause is an expression evaluated per segment; identifiers in the expression refer to other lenses (or the lens itself by name).

`LIMIT` caps the number of returned segments. `OFFSET` skips the first `m` segments (applied after `LIMIT`). Use both together for pagination.

Response format: `RANGE <n>; <start>:<end>:<value>; ...` where `n` is the number of segments.

Requires `R` permission.

### `REDUCE LENS <name> <start> <end> USING <func>`

Aggregates all intervals in `[start, end)` using the named function. The result is a scalar value. Works on both base and derived lenses.

```
REDUCE LENS cpu 0 86400 USING avg
→ VAL f36.77

REDUCE LENS cpu 0 86400 USING max
→ VAL i82

REDUCE LENS requests 0 43200 USING sum
→ VAL i904825
```

| function | behaviour |
|----------|-----------|
| `avg` | time-weighted mean (weighted by interval duration) |
| `min` | minimum value across all intervals |
| `max` | maximum value across all intervals |
| `sum` | sum of all values (not time-weighted) |
| `count` | number of intervals |

Requires `R` permission.

### `HISTORY LENS <name> [<start> <end>]`

Lists all layers for a lens, optionally filtered to those whose time range overlaps `[start, end)`. Returns layer metadata: ID, write timestamp, earliest start, and latest end.

```
HISTORY LENS cpu
→ LAYERS 3; 1:1717000000000:0:3600; 2:1717001000000:3600:7200; 3:1717002000000:7200:10800

HISTORY LENS cpu 3600 7200
→ LAYERS 1; 2:1717001000000:3600:7200
```

Response format: `LAYERS <n>; <id>:<written_at>:<min_start>:<max_end>; …`

- `id` — monotonic layer identifier assigned at write time
- `written_at` — wall-clock milliseconds since Unix epoch, restored verbatim across restarts
- `min_start` — earliest tau start in the layer
- `max_end` — latest tau end in the layer

The per-layer tau count is not carried on the wire — it stays server-side in the `LayerInfo` struct.

Requires `R` permission.

---

## Database backup and restore

### `BACKUP DATABASE <name> TO "<path>"`

Writes a snapshot of the named database to a WAL file at `path`. The file includes all schema DDL (CREATE LENS, DERIVE LENS) and all data layers. An existing file at `path` is replaced atomically.

```
BACKUP DATABASE sensors TO "/backups/sensors-2026-01-01.bak"
→ OK
```

Requires `R` permission on the named database.

### `RESTORE DATABASE <name> FROM "<path>"`

Reads a backup file and creates a new in-memory database named `name`. The backup must have been produced by `BACKUP DATABASE`. The database name must not already exist in the executor.

```
RESTORE DATABASE sensors FROM "/backups/sensors-2026-01-01.bak"
→ OK
```

Requires global admin (`A on *`).

---

## Users and permissions

These statements require `--auth` to be enabled on the server.

### `CREATE USER <name> PASSWORD "<pass>"`

Creates a user. The password is hashed with Argon2id and stored in the user file.

```
CREATE USER alice PASSWORD "hunter2"
→ OK
```

Requires global admin.

### `DROP USER <name>`

Removes a user.

```
DROP USER alice
→ OK
```

Requires global admin.

### `GRANT <perms> ON <db|*> TO <user>`

Grants permissions to a user on a specific database, or on `*` (all databases including future ones).

```
GRANT R ON metrics TO alice
→ OK

GRANT CRUD ON * TO alice
→ OK

GRANT A ON * TO alice    ← promotes alice to global admin
→ OK
```

Permission bits: `C` (Create), `R` (Read), `U` (Update/write), `D` (Delete), `A` (Admin). Use `*` to grant all bits, `-` to grant none.

Requires `A` on the target database, or global admin.

### `REVOKE <perms> ON <db|*> FROM <user>`

Revokes permissions.

```
REVOKE U ON staging FROM alice
→ OK
```

Requires `A` on the target database, or global admin.

### `SHOW USERS`

Lists all usernames.

```
SHOW USERS
→ NAMES 2; admin; alice
```

Requires global admin.

### `SHOW GRANTS [<user>]`

Shows every user's grants, or just the named user's grants. Non-admin users may view their own grants.

```
SHOW GRANTS alice
→ GRANTS 1; alice metrics:R staging:U

SHOW GRANTS
→ GRANTS 2; admin *:A; alice metrics:R staging:U
```

---

## Expressions

Expressions appear in `DERIVE LENS AS <expr>` and in `RANGE ... WHERE <expr>`.

### Operators

Precedence from low (evaluated last) to high (evaluated first):

| precedence | operators |
|------------|-----------|
| 1 (lowest) | `\|\|` |
| 2 | `&&` |
| 3 | `==` `!=` `<` `<=` `>` `>=` |
| 4 | `+` `-` |
| 5 | `*` `/` `%` |
| 6 | unary `-` `!` |
| 7 (highest) | primary |

### Primary expressions

- **Literals:** integers (`42`, `-5`), floats (`3.14`, `-0.5`), booleans (`true`, `false`), strings (`"hello"`), null (`null`)
- **Identifiers:** reference another lens by name, e.g. `cpu`, `temperature`
- **Parentheses:** `(expr)` for explicit grouping
- **Aggregation calls:** `func(lens, rel_start, rel_end)` (see Aggregations below)

### Aggregation expressions

Rolling-window aggregations are first-class expression nodes, available in both `DERIVE` and `WHERE`:

```
avg(lens, rel_start, rel_end)
min(lens, rel_start, rel_end)
max(lens, rel_start, rel_end)
sum(lens, rel_start, rel_end)
count(lens, rel_start, rel_end)
```

`rel_start` and `rel_end` are offsets **relative to the evaluation timestamp `t`**. The window evaluated is `[t + rel_start, t + rel_end)`.

```
avg(cpu, -600, 0)   ← time-weighted average of cpu over the 600 units before t
avg(cpu, 0, 600)    ← time-weighted average of cpu over the 600 units after t
min(cpu, -3600, 0)  ← minimum cpu over the last hour
```

---

## Response codes

| prefix | meaning |
|--------|---------|
| `OK` | DDL or write succeeded |
| `OK BYE` | Server acknowledged `QUIT` / `EXIT` |
| `VAL <v>` | Scalar from `AT` or `REDUCE` |
| `VAL NIL` | Point lookup: no interval covers the timestamp |
| `RANGE <n>; <s>:<e>:<v>; ...` | `n` segments from `RANGE` |
| `NAMES <n>; <name>; ...` | List from `SHOW DATABASES`, `SHOW LENSES`, `SHOW USERS` |
| `GRANTS <n>; <user> <db>:<perms>; ...` | Output of `SHOW GRANTS` |
| `LAYERS <n>; <id>:<written_at>:<min>:<max>; ...` | Layer metadata from `HISTORY LENS` |
| `STATUS <n>; <key>:<value>; ...` | Key-value pairs from `SHOW STATUS` |
| `ERR <message>` | Parse, executor, or permission error |

### Value encoding

| type | encoding | example |
|------|----------|---------|
| int | `i<value>` | `i42` |
| float | `f<value>` | `f3.14` |
| string | `s<percent-encoded>` | `shello%20world` |
| bool | `b0` or `b1` | `b1` |
| null | `n` | `n` |

---

## Grammar summary

```
statement :=
    CREATE DATABASE name
  | DROP DATABASE name
  | SHOW DATABASES
  | SHOW STATUS
  | CREATE LENS name type
  | DROP LENS name
  | DERIVE LENS name AS expr
  | SET TTL LENS name secs
  | UNSET TTL LENS name
  | APPEND LENS name tau_list
  | BATCH APPEND LENS name { tau_block }
  | COPY LENS name FROM "path"
  | AT LENS name timestamp
  | AT LENS name timestamp AS OF written_at
  | AT LENS name timestamp LAYER layer_id
  | RANGE LENS name start end [WHERE expr] [LIMIT n [OFFSET m]]
  | REDUCE LENS name start end USING func
  | HISTORY LENS name [start end]
  | BACKUP DATABASE name TO "path"
  | RESTORE DATABASE name FROM "path"
  | START TRANSACTION
  | COMMIT
  | ROLLBACK
  | CREATE USER name PASSWORD "pass"
  | DROP USER name
  | GRANT perms ON db|* TO user
  | REVOKE perms ON db|* FROM user
  | SHOW USERS
  | SHOW GRANTS [user]
  | AUTH user pass
  | QUIT | EXIT

type      := int | float | str | bool | bytes
func      := min | max | avg | sum | count
perms     := any combination of C R U D A, or *, or -
tau_list  := s e v [, s e v ...]
tau_block := s e v [; s e v ...] [;]
```
