# ql - Query Language

The grammar, AST, and parser for Tau's query language.

## What it is

A small, purpose-built query language for temporal interval data. It is deliberately not SQL - there are no joins, no tables, and no `GROUP BY`. Every statement operates on a single named lens, which fits the time-series use case: you almost always want to ask about one signal at a time.

## Grammar overview

```
stmt   := create | append | copy | derive | show | at | range | reduce
        | drop | use | user_admin | start | commit | rollback

create := CREATE DATABASE <name>
        | CREATE LENS <name> <type>
        | CREATE USER <name> PASSWORD "<pass>"
append := APPEND LENS <name> <s> <e> <v> [, <s> <e> <v> …]
copy   := COPY LENS <name> FROM "<path>"
derive := DERIVE LENS <name> AS <expr>
show   := SHOW DATABASES
        | SHOW LENSES
        | SHOW USERS
        | SHOW GRANTS [<user>]
at     := AT LENS <name> <timestamp>
range  := RANGE LENS <name> <start> <end> [WHERE <expr>]
reduce := REDUCE LENS <name> <start> <end> USING <func>
drop   := DROP LENS <name>
        | DROP DATABASE <name>
        | DROP USER <name>
use    := USE DATABASE <name>
start  := START TRANSACTION
commit := COMMIT
rollback := ROLLBACK

user_admin
       := GRANT <perms> ON <db | *> TO <user>
        | REVOKE <perms> ON <db | *> FROM <user>

type   := int | float | str | bool | bytes
func   := min | max | avg | sum | count
perms  := one or more of C R U D A, in any order, case-insensitive
        | * (alias for CRUDA)
        | - (alias for the empty set)
```

Keywords are case-insensitive. Identifiers and string literals are not.

Expressions have standard C-style operator precedence: `||` < `&&` < comparisons < additive < multiplicative < unary. Aggregation calls (`avg(lens, rel_start, rel_end)`) bind as primary expressions.

### Bulk `APPEND`

`APPEND LENS name s0 e0 v0, s1 e1 v1, …` batches multiple taus into a single layer write. All taus are validated before any data is written - a type mismatch on any entry rejects the whole statement. This is the preferred form for loading a time series in one shot; it is equivalent to `COPY` for in-memory data.

### `COPY` CSV ingestion

`COPY LENS name FROM "/path/to/file.csv"` reads a CSV file where every non-blank, non-comment (`#`) line is `start,end,value`. The entire file is parsed and appended as a single layer, so the ingest is atomic from the perspective of concurrent readers. The path is resolved on the **server's** filesystem - use `tauctl`'s `load` command instead when the file lives on the client.

### Transactions

`START TRANSACTION` begins a transaction on the current connection. All subsequent mutations (`APPEND`, `COPY`, `CREATE LENS`, `DERIVE LENS`, `DROP LENS`) are buffered in memory and invisible to every other connection until `COMMIT` is issued. `ROLLBACK` discards the buffer without applying anything.

```
START TRANSACTION
APPEND LENS cpu 0 3600 42
APPEND LENS cpu 3600 7200 55
COMMIT              ← both appends applied atomically at this point
```

Error cases:
- `START TRANSACTION` while a transaction is already active → `ERR transaction already active`
- `COMMIT` or `ROLLBACK` with no active transaction → `ERR no active transaction`

Transactions are scoped to a single client connection. There is no distributed transaction manager, no support for isolation levels, and no cross-connection conflict detection. The server processes each transaction serially; the write lock is held for the entire commit so readers see either zero or all of the batch's writes.

### User and permission management

`CREATE USER`, `DROP USER`, `GRANT`, `REVOKE`, `SHOW USERS`, and `SHOW GRANTS` are first-class statements gated on the global-admin bit (`A` on `*`). `GRANT R ON main TO alice` unions the `R` bit into alice's existing `main` grants; `REVOKE` clears them and prunes the entry when it becomes empty. The wildcard database `*` applies to every existing and future database. Promotion to global admin is just `GRANT A ON * TO <user>`.

## AST (`ast.rs`)

The AST is intentionally shallow. There is no separate `Query` node or `Plan` node - the parser produces a `Stmt` directly and the executor acts on it. This is fine because there is no query optimiser; execution is always a direct walk of the AST.

`Expr::Agg` is the notable case: it embeds a relative window `[rel_start, rel_end)` rather than absolute timestamps. The executor shifts these to absolute positions at evaluation time using the current lookup timestamp `t`. This is what makes rolling aggregations like `avg(temp, -60, 0)` work naturally in `DERIVE` expressions.

## Parser (`parser.rs`)

Built on `nom` 8, using combinator-style parsing. The entry point is `parse(input) -> IResult<&str, Stmt>`.

The parser is intentionally not streaming - it expects a complete statement as input. This matches the wire protocol, where the server reads one newline-terminated line and hands it to the parser. There is no partial-parse or continuation mode.

### Why nom rather than a generated parser?

`nom` keeps the parser as ordinary Rust code, making it easy to add new syntax without a separate grammar file or build step. The combinator style also makes error attribution straightforward - `nom` error messages include the position in the input where parsing failed. A yacc-style generator would be overkill for a grammar this size.

### Error handling

`nom` returns `IResult` which carries the remaining unparsed input on success. The server rejects any query where the remaining input is non-empty after parsing - this is the "trailing input" error. It is a deliberate strictness: a client that sends `CREATE LENS x int JUNK` should know it made a mistake rather than silently having the `JUNK` ignored.

## Design decisions

### Type declarations are hints, not enforcement at the storage level

`CREATE LENS x int` records that `x` has type `int`. This is enforced by the executor at append time: a float value will be rejected. But the storage engine itself is generic - it can hold any `V`. The type information lives only in the executor's `DbState::base_types` map.

`CREATE LENS` and `DERIVE LENS` statements are written as `S:` schema lines in the WAL when WAL mode is active. On restart, `Wal::replay_schemas` returns these lines and the executor replays them before accepting new writes, so both data and schema survive a restart.

### `null` is always permitted

Appending `null` to an `int` lens is allowed. This is the correct behaviour for a time-series that has gaps: a null tau explicitly records "no value over this interval," which is different from "no tau exists here." The executor propagates null as `None` through derived expressions - if a source lens is null, derived lenses that depend on it also return none.

### Cycle detection in derived lenses

`DERIVE LENS z AS z + 1` or any chain that eventually references itself is rejected at definition time with `CycleDetected`. The executor performs a DFS through the existing derived graph starting from the new expression before inserting it. The `visited` set prevents the DFS from looping even if the graph were already cyclic (e.g. from a pre-existing data file).

### `REDUCE` vs. aggregation in `DERIVE`

`REDUCE LENS x 0 100 USING avg` computes a scalar over an absolute range. The `avg(x, rel_start, rel_end)` form in expressions computes a scalar over a sliding window relative to the evaluation timestamp. They use the same underlying `eval_agg` function; `REDUCE` is syntactic sugar for a standalone aggregate query.
