# ql — Query Language

## What it is

The TauQL grammar, AST, and `nom` parser. It is a plain library deliberately outside the kernel: parsing produces a `Stmt`, and the kernel routes that statement to the service that owns it.

## How it works

`Stmt` is the top-level statement enum (one variant per keyword group) with `is_read_only` driving kernel routing. `Expr` is the expression tree (`Lit`, `Ident`, `Unary`, `Binary`, `Agg`) that the query service evaluates directly. `Type` is the declared lens type (`Int`, `Float`, `Str`, `Bool`); `Literal` carries embedded values (`Arc<str>`-backed strings so clones are pointer bumps); `AggFunc` is `Min`/`Max`/`Avg`/`Sum`/`Count`.

Operator precedence (low to high): `||`, `&&`, comparison, `+ -`, `* / %`, unary, primary. Statement keywords are UPPERCASE-only; type names, aggregate functions and value literals (`true`/`false`/`null`) are lowercase-only; identifiers are case-sensitive. Aggregation expressions (`avg(lens, rel_start, rel_end)` …) are first-class nodes inside `DERIVE LENS` and `WHERE`. The full grammar lives in the doc-comment at the top of `ast.rs`.

## Using it

`parse(input)` parses one complete statement; `parse_literal(s)` parses a single scalar for bulk-load paths; `format_parse_error` renders a column-anchored message instead of nom's debug output. `needs_registry_lock(stmt)` flags statements that mutate the database registry (`CREATE`/`DROP`/`USE DATABASE`, user management, transactions, `RESTORE`) — the kernel's db service takes its registry write lock for these.
