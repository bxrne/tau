+++
title = "TauQL Language Reference"
date = 2026-05-28
template = "page.html"
+++

# TauQL Language Reference

TauQL is a line-oriented command language. **One statement in, one response line out.** Keywords are case-insensitive. Identifiers are case-sensitive.

---

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

### `USE DATABASE <name>`

Sets the active database for subsequent lens operations.

```
USE DATABASE sensors
→ OK
```

Requires any grant on the named database.

### `SHOW DATABASES`

Lists all database names the caller has access to.

```
SHOW DATABASES
→ NAMES 2; metrics; sensors
```

Global admins see all databases. Non-admin users see only databases they hold at least one grant on.

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

### `DERIVE LENS <name> AS <expr>`

Creates a derived lens. The expression is compiled into a lazy closure; nothing is materialised. Every query re-evaluates the expression at the requested timestamp.

```
DERIVE LENS fahrenheit AS celsius * 9.0 / 5.0 + 32.0
→ OK

DERIVE LENS cpu_smooth AS avg(cpu, -600, 0)
→ OK

DERIVE LENS cpu_hot AS cpu > avg(cpu, -1800, 0)
→ OK
```

Cycle detection runs at `DERIVE` time. If the expression would create a dependency cycle, the statement is rejected with `ERR cycle detected`.

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

Interval semantics: `s` (start, inclusive), `e` (end, exclusive), `v` (value). Every interval in a single batch must be non-overlapping.

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

### `RANGE LENS <name> <start> <end> [WHERE <expr>]`

Range scan over the interval `[start, end)`. Returns all temporal segments within that window.

```
RANGE LENS cpu 0 3600
→ RANGE 60; 0:60:i45; 60:120:i72; ...

RANGE LENS cpu 0 86400 WHERE cpu > 70
→ RANGE 12; 44340:44400:i75; ...
```

Adjacent segments with the same value are merged automatically. The `WHERE` clause is an expression evaluated per segment; identifiers in the expression refer to other lenses (or the lens itself by name).

Response format: `RANGE <n>; <start>:<end>:<value>; ...` where `n` is the number of segments.

Requires `R` permission.

### `REDUCE LENS <name> <start> <end> USING <func>`

Aggregates all intervals in `[start, end)` using the named function. The result is a scalar value.

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
  | USE DATABASE name
  | SHOW DATABASES
  | SHOW LENSES
  | CREATE LENS name type
  | DROP LENS name
  | DERIVE LENS name AS expr
  | APPEND LENS name tau_list
  | COPY LENS name FROM "path"
  | AT LENS name timestamp
  | RANGE LENS name start end [WHERE expr]
  | REDUCE LENS name start end USING func
  | CREATE USER name PASSWORD "pass"
  | DROP USER name
  | GRANT perms ON db|* TO user
  | REVOKE perms ON db|* FROM user
  | SHOW USERS
  | SHOW GRANTS [user]
  | AUTH user pass
  | QUIT | EXIT

type := int | float | str | bool | bytes
func := min | max | avg | sum | count
perms := any combination of C R U D A, or *, or -
tau_list := s e v [, s e v ...]
```
