# ql — Query Language

The grammar, AST, and parser for TauQL.

## Entry points

| Symbol | Purpose |
|--------|---------|
| `parse(input) -> IResult<&str, Stmt>` | Parse one complete statement |
| `parse_literal(s) -> Option<Literal>` | Parse a single scalar literal; used by bulk-load paths instead of constructing a full statement |
| `needs_registry_lock(stmt) -> bool` | Returns `true` for statements that need the global executor write lock (CREATE DATABASE, DROP DATABASE, USE DATABASE, user management, transactions, RESTORE) |

## Grammar summary

Operator precedence (low to high): `||`, `&&`, comparison, `+ -`, `* /  %`, unary, primary. Statement keywords are UPPERCASE-only; type names, aggregate functions and value literals (`true`/`false`/`null`) are lowercase-only. Identifiers are case-sensitive.

Aggregation expressions (`avg(lens, rel_start, rel_end)` etc.) are first-class nodes available inside `DERIVE LENS` and `WHERE` clauses.

The full grammar is in the doc-comment at the top of `ast.rs`.

## AST types

`Stmt` — top-level statement enum. Every variant maps to one TauQL keyword group.

`Expr` — expression tree: `Lit`, `Ident`, `Unary`, `Binary`, `Agg`. The evaluator (`libtau::query`) works directly on this tree.

`Type` — declared type for `CREATE LENS`: `Int`, `Float`, `Str`, `Bool`, `Bytes`.

`Literal` — embedded values: `Int(i64)`, `Float(f64)`, `Str(Arc<str>)`, `Bool`, `Null`. Uses `Arc<str>` so cloning an expression (e.g. for re-evaluation per query window) is an atomic pointer bump.

`AggFunc` — `Min`, `Max`, `Avg`, `Sum`, `Count`.
