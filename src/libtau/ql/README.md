# ql — Query Language

The grammar, AST, and parser for Tau's query language.

## What it is

A small, purpose-built query language for temporal interval data. It is deliberately not SQL — there are no joins, no tables, and no `GROUP BY`. Every statement operates on a single named lens, which fits the time-series use case: you almost always want to ask about one signal at a time.

## Grammar overview

```
stmt   := create | append | derive | at | range | reduce | drop | use

create := CREATE DATABASE <name>
        | CREATE LENS <name> <type>
append := APPEND LENS <name> <start> <end> <value>
derive := DERIVE LENS <name> AS <expr>
at     := AT LENS <name> <timestamp>
range  := RANGE LENS <name> <start> <end> [WHERE <expr>]
reduce := REDUCE LENS <name> <start> <end> USING <func>
drop   := DROP LENS <name>
        | DROP DATABASE <name>
use    := USE DATABASE <name>

type   := int | float | str | bool | bytes
func   := min | max | avg | sum | count
```

Keywords are case-insensitive. Identifiers and string literals are not.

Expressions have standard C-style operator precedence: `||` < `&&` < comparisons < additive < multiplicative < unary. Aggregation calls (`avg(lens, rel_start, rel_end)`) bind as primary expressions.

## AST (`ast.rs`)

The AST is intentionally shallow. There is no separate `Query` node or `Plan` node — the parser produces a `Stmt` directly and the executor acts on it. This is fine because there is no query optimiser; execution is always a direct walk of the AST.

`Expr::Agg` is the notable case: it embeds a relative window `[rel_start, rel_end)` rather than absolute timestamps. The executor shifts these to absolute positions at evaluation time using the current lookup timestamp `t`. This is what makes rolling aggregations like `avg(temp, -60, 0)` work naturally in `DERIVE` expressions.

## Parser (`parser.rs`)

Built on `nom` 8, using combinator-style parsing. The entry point is `parse(input) -> IResult<&str, Stmt>`.

The parser is intentionally not streaming — it expects a complete statement as input. This matches the wire protocol, where the server reads one newline-terminated line and hands it to the parser. There is no partial-parse or continuation mode.

### Why nom rather than a generated parser?

`nom` keeps the parser as ordinary Rust code, making it easy to add new syntax without a separate grammar file or build step. The combinator style also makes error attribution straightforward — `nom` error messages include the position in the input where parsing failed. A yacc-style generator would be overkill for a grammar this size.

### Error handling

`nom` returns `IResult` which carries the remaining unparsed input on success. The server rejects any query where the remaining input is non-empty after parsing — this is the "trailing input" error. It is a deliberate strictness: a client that sends `CREATE LENS x int JUNK` should know it made a mistake rather than silently having the `JUNK` ignored.

## Design decisions

### Type declarations are hints, not enforcement at the storage level

`CREATE LENS x int` records that `x` has type `int`. This is enforced by the executor at append time: a float value will be rejected. But the storage engine itself is generic — it can hold any `V`. The type information lives only in the executor's `DbState::base_types` map.

This has a current consequence: **lens type declarations are not persisted to the WAL**, so they are lost on restart. The executor must be taught to write `CREATE LENS` events to the WAL during startup recovery. This is tracked in the roadmap.

### `null` is always permitted

Appending `null` to an `int` lens is allowed. This is the correct behaviour for a time-series that has gaps: a null tau explicitly records "no value over this interval," which is different from "no tau exists here." The executor propagates null as `None` through derived expressions — if a source lens is null, derived lenses that depend on it also return none.

### `REDUCE` vs. aggregation in `DERIVE`

`REDUCE LENS x 0 100 USING avg` computes a scalar over an absolute range. The `avg(x, rel_start, rel_end)` form in expressions computes a scalar over a sliding window relative to the evaluation timestamp. They use the same underlying `eval_agg` function; `REDUCE` is syntactic sugar for a standalone aggregate query.
