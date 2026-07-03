//! `nom`-based parser for the Tau query language.
//!
//! Entry point is [`parse`], which returns a single [`Stmt`].  Operator
//! precedence (lowest → highest) is:
//!
//! - `||`
//! - `&&`
//! - `==`, `!=`, `<`, `<=`, `>`, `>=`
//! - `+`, `-`
//! - `*`, `/`, `%`
//! - unary `-`, `!`
//! - literals, identifiers, `(` expr `)`

use std::sync::Arc;

use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{is_not, tag, take_while1},
    character::complete::{alpha1, alphanumeric1, char, digit1, multispace0, multispace1},
    combinator::{map, map_res, opt, recognize, value},
    multi::{many0, separated_list0, separated_list1},
    sequence::{delimited, pair, preceded},
};

use super::ast::{AggFunc, BinOp, Expr, Literal, Stmt, Type, UnOp};
use crate::services::auth::Perm;

/// Turn a `nom` parse failure into a human-readable, single-line message that
/// points at the column where parsing stalled.  `nom`'s own `Display`/`Debug`
/// output (`Parsing Error: Error { input: "...", code: Tag }`) leaks internal
/// combinator names and a confusing offset, so it is never surfaced to clients.
pub fn format_parse_error(query: &str, err: nom::Err<nom::error::Error<&str>>) -> String {
    let remaining = match &err {
        nom::Err::Error(e) | nom::Err::Failure(e) => e.input,
        nom::Err::Incomplete(_) => return "parse error: unexpected end of input".to_string(),
    };
    // The error slice is always a suffix of `query`; its start is the column
    // at which the last combinator gave up.
    let col = query.len().saturating_sub(remaining.len());
    let near = remaining.trim_start();
    if near.is_empty() {
        format!("parse error at column {}: unexpected end of input", col + 1)
    } else {
        let snippet: String = near.chars().take(24).collect();
        format!("parse error at column {}: near `{snippet}`", col + 1)
    }
}

/// Parse a single statement.  Trailing whitespace is consumed but trailing
/// crap is reported as an error.
pub fn parse(input: &str) -> IResult<&str, Stmt> {
    let (input, _) = multispace0(input)?;
    let (input, s) = alt((
        stmt_create,
        stmt_batch_append,
        stmt_append,
        stmt_copy,
        stmt_xderive,
        stmt_derive,
        stmt_at,
        stmt_range,
        stmt_reduce,
        stmt_drop,
        stmt_use,
        stmt_show,
        stmt_grant,
        stmt_revoke,
        stmt_start_tx,
        stmt_commit,
        stmt_rollback,
        stmt_history,
        stmt_backup,
        stmt_restore,
        alt((stmt_set_ttl, stmt_unset_ttl)),
    ))
    .parse(input)?;
    let (input, _) = multispace0(input)?;
    Ok((input, s))
}

// `CREATE LENS <name> <type>`, `CREATE DATABASE <name>`, or
// `CREATE USER <name> PASSWORD "<pass>"`.
fn stmt_create(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("CREATE").parse(i)?;
    alt((stmt_create_lens, stmt_create_database, stmt_create_user)).parse(i)
}

fn stmt_create_user(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("USER").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag("PASSWORD")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, password) = string_lit(i)?;
    Ok((i, Stmt::CreateUser { name, password }))
}

fn stmt_create_lens(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, ty) = type_name(i)?;
    // Optional `AXES (<axis>, …)` — declares the lens arity; axis 0 is valid time.
    let (i, axes) = opt(preceded(
        (
            multispace1,
            tag("AXES"),
            multispace0,
            char('('),
            multispace0,
        ),
        (
            separated_list1(delimited(multispace0, char(','), multispace0), ident),
            preceded(multispace0, char(')')),
        ),
    ))
    .parse(i)?;
    Ok((
        i,
        Stmt::Create {
            name,
            ty,
            axes: axes.map(|(names, _)| names).unwrap_or_default(),
        },
    ))
}

fn stmt_create_database(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::CreateDatabase { name }))
}

fn stmt_start_tx(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("START").parse(i)?;
    let (i, _) = tag("TRANSACTION")(i)?;
    Ok((i, Stmt::StartTransaction))
}

fn stmt_commit(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = tag("COMMIT")(i)?;
    Ok((i, Stmt::Commit))
}

fn stmt_rollback(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = tag("ROLLBACK")(i)?;
    Ok((i, Stmt::Rollback))
}

/// Parse a single `start end value` triple (no comma prefix).
fn tau_triple(i: &str) -> IResult<&str, (i64, i64, Literal)> {
    let (i, start) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, end) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, value) = literal(i)?;
    Ok((i, (start, end, value)))
}

/// One bracketed axis interval: `[lo hi]` (half-open at query time).
fn axis_interval(i: &str) -> IResult<&str, (i64, i64)> {
    let (i, _) = char('[')(i)?;
    let (i, _) = multispace0(i)?;
    let (i, lo) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, hi) = integer(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char(']')(i)?;
    Ok((i, (lo, hi)))
}

/// Per-axis `(lo, hi)` coordinates plus the value — one parsed N-D tau.
type NdBoxLit = (Vec<(i64, i64)>, Literal);

/// One N-dimensional box: `[lo hi] [lo hi] … value`.
fn nd_box(i: &str) -> IResult<&str, NdBoxLit> {
    let (i, coords) = separated_list1(multispace1, axis_interval).parse(i)?;
    let (i, _) = multispace1(i)?;
    let (i, value) = literal(i)?;
    Ok((i, (coords, value)))
}

fn stmt_append(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("APPEND").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    // N-dimensional form: bracketed boxes, `[0 10] [5 15] 42 [, …]`.
    if i.starts_with('[') {
        let (i, taus) =
            separated_list1(delimited(multispace0, char(','), multispace0), nd_box).parse(i)?;
        return Ok((i, Stmt::AppendNd { name, taus }));
    }
    let (i, first) = tau_triple(i)?;
    // Optional additional taus: ", start end value"
    let (i, rest) = many0(preceded(
        delimited(multispace0, char(','), multispace0),
        tau_triple,
    ))
    .parse(i)?;
    let mut taus = vec![first];
    taus.extend(rest);
    Ok((i, Stmt::Append { name, taus }))
}

/// `<word> "<path>"` suffix shared by COPY FROM / BACKUP TO / RESTORE FROM.
fn path_suffix<'a>(i: &'a str, word: &'static str) -> IResult<&'a str, String> {
    let (i, _) = (multispace1, tag(word), multispace1).parse(i)?;
    string_lit(i)
}

/// `COPY LENS <name> FROM "<path>"` - bulk-ingest from a CSV file.
fn stmt_copy(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("COPY").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, path) = path_suffix(i, "FROM")?;
    Ok((i, Stmt::Copy { name, path }))
}

/// `SHOW DATABASES`, `SHOW LENSES`, `SHOW USERS`, `SHOW GRANTS [<name>]`, or
/// `SHOW STATUS`.
fn stmt_show(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("SHOW").parse(i)?;
    alt((
        value(Stmt::ShowDatabases, tag("DATABASES")),
        value(Stmt::ShowLenses, tag("LENSES")),
        value(Stmt::ShowUsers, tag("USERS")),
        value(Stmt::ShowStatus, tag("STATUS")),
        stmt_show_grants,
    ))
    .parse(i)
}

fn stmt_show_grants(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = tag("GRANTS")(i)?;
    let (i, user) = opt(preceded(multispace1, ident)).parse(i)?;
    Ok((i, Stmt::ShowGrants { user }))
}

/// Optional `OVER <start> <end>` domain clause shared by DERIVE / XDERIVE.
fn over_clause(i: &str) -> IResult<&str, (i64, i64)> {
    let (i, _) = (multispace1, tag("OVER"), multispace1).parse(i)?;
    let (i, start) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, end) = integer(i)?;
    Ok((i, (start, end)))
}

fn stmt_derive(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DERIVE").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag("AS")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, e) = expr(i)?;
    let (i, range) = opt(over_clause).parse(i)?;
    Ok((
        i,
        Stmt::Derive {
            name,
            expr: e,
            range,
        },
    ))
}

/// `XDERIVE LENS <name> AS <expr> [OVER <start> <end>]` - materialised lens.
fn stmt_xderive(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("XDERIVE").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag("AS")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, e) = expr(i)?;
    let (i, range) = opt(over_clause).parse(i)?;
    Ok((
        i,
        Stmt::Xderive {
            name,
            expr: e,
            range,
        },
    ))
}

fn stmt_at(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("AT").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, t) = integer(i)?;

    // Further integers make this an N-dimensional point query: one coordinate
    // per declared axis. Keywords (`AS`, `LAYER`) are not integers, so the
    // single-axis forms below are untouched.
    let (i, more) = many0(preceded(multispace1, integer)).parse(i)?;
    if !more.is_empty() {
        let mut ts = vec![t];
        ts.extend(more);
        let (i, as_of) = opt(preceded(
            (multispace1, tag("AS"), multispace1, tag("OF"), multispace1),
            integer,
        ))
        .parse(i)?;
        return Ok((i, Stmt::AtNd { name, ts, as_of }));
    }

    // Try "AS OF <timestamp>".
    let (i, as_of) = opt(preceded(
        (multispace1, tag("AS"), multispace1, tag("OF"), multispace1),
        integer,
    ))
    .parse(i)?;
    if let Some(ts) = as_of {
        return Ok((i, Stmt::AtAsOf { name, t, as_of: ts }));
    }

    // Try "LAYER <id>".
    let (i, layer_id) = opt(preceded(
        (multispace1, tag("LAYER"), multispace1),
        unsigned_integer,
    ))
    .parse(i)?;
    if let Some(lid) = layer_id {
        return Ok((
            i,
            Stmt::AtLayer {
                name,
                t,
                layer_id: lid,
            },
        ));
    }

    Ok((i, Stmt::At { name, t }))
}

fn stmt_range(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("RANGE").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, start) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, end) = integer(i)?;
    // N-dimensional form: `AT (<t1>, …)` fixes every non-valid axis at a point
    // while valid time sweeps `[start, end)`.
    let (i, fixed) = opt(preceded(
        (multispace1, tag("AT"), multispace0, char('('), multispace0),
        (
            separated_list1(delimited(multispace0, char(','), multispace0), integer),
            preceded(multispace0, char(')')),
        ),
    ))
    .parse(i)?;
    if let Some((fixed, _)) = fixed {
        return Ok((
            i,
            Stmt::RangeNd {
                name,
                start,
                end,
                fixed,
            },
        ));
    }
    let (i, filter) = opt(preceded((multispace1, tag("WHERE"), multispace1), expr)).parse(i)?;
    let (i, limit) = opt(map(
        preceded((multispace1, tag("LIMIT"), multispace1), unsigned_integer),
        |n| n as usize,
    ))
    .parse(i)?;
    let (i, offset) = opt(map(
        preceded((multispace1, tag("OFFSET"), multispace1), unsigned_integer),
        |n| n as usize,
    ))
    .parse(i)?;
    Ok((
        i,
        Stmt::Range {
            name,
            start,
            end,
            filter,
            limit,
            offset,
        },
    ))
}

/// `DROP LENS <name>`, `DROP DATABASE <name>`, or `DROP USER <name>`.
fn stmt_drop(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DROP").parse(i)?;
    alt((stmt_drop_lens, stmt_drop_database, stmt_drop_user)).parse(i)
}

fn stmt_drop_lens(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::Drop { name }))
}

fn stmt_drop_database(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::DropDatabase { name }))
}

fn stmt_drop_user(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("USER").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::DropUser { name }))
}

/// Shared body of GRANT/REVOKE: `<perms> ON <db|*> <prep> <user>`.
fn perm_clause<'a>(i: &'a str, prep: &'static str) -> IResult<&'a str, (Perm, String, String)> {
    let (i, perms) = perm_letters(i)?;
    let (i, _) = (multispace1, tag("ON"), multispace1).parse(i)?;
    let (i, database) = db_target(i)?;
    let (i, _) = (multispace1, tag(prep), multispace1).parse(i)?;
    let (i, user) = ident(i)?;
    Ok((i, (perms, database, user)))
}

/// `GRANT <perms> ON <db|*> TO <user>`.
fn stmt_grant(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("GRANT").parse(i)?;
    let (i, (perms, database, user)) = perm_clause(i, "TO")?;
    Ok((
        i,
        Stmt::Grant {
            perms,
            database,
            user,
        },
    ))
}

/// `REVOKE <perms> ON <db|*> FROM <user>`.
fn stmt_revoke(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("REVOKE").parse(i)?;
    let (i, (perms, database, user)) = perm_clause(i, "FROM")?;
    Ok((
        i,
        Stmt::Revoke {
            perms,
            database,
            user,
        },
    ))
}

/// A run of CRUDA letters, or the literal `*` (all), or `-` (none).
fn perm_letters(i: &str) -> IResult<&str, Perm> {
    let (i, s) = alt((
        recognize(char('*')),
        recognize(char('-')),
        take_while1(|c: char| c.is_ascii_alphabetic()),
    ))
    .parse(i)?;
    let p = Perm::parse(s)
        .map_err(|_| nom::Err::Error(nom::error::Error::new(i, nom::error::ErrorKind::Tag)))?;
    Ok((i, p))
}

/// Database target: identifier or `*`.
fn db_target(i: &str) -> IResult<&str, String> {
    alt((map(tag("*"), |s: &str| s.to_string()), ident)).parse(i)
}

/// `REDUCE LENS <name> <start> <end> USING <func>` - aggregate over a range.
fn stmt_reduce(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("REDUCE").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, start) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, end) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag("USING")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, func) = agg_func(i)?;
    Ok((
        i,
        Stmt::Reduce {
            name,
            start,
            end,
            func,
        },
    ))
}

/// `SET TTL LENS <name> <secs>` — configure per-lens TTL (seconds).
fn stmt_set_ttl(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("SET").parse(i)?;
    let (i, _) = kw("TTL").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, secs) = integer(i)?;
    Ok((i, Stmt::SetTtl { name, secs }))
}

/// `UNSET TTL LENS <name>` — remove the per-lens TTL policy.
fn stmt_unset_ttl(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("UNSET").parse(i)?;
    let (i, _) = kw("TTL").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::UnsetTtl { name }))
}

/// `USE DATABASE <name>` - switches the active database.
fn stmt_use(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("USE").parse(i)?;
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::UseDatabase { name }))
}

fn agg_func(i: &str) -> IResult<&str, AggFunc> {
    alt((
        value(AggFunc::Min, tag("min")),
        value(AggFunc::Max, tag("max")),
        value(AggFunc::Avg, tag("avg")),
        value(AggFunc::Sum, tag("sum")),
        value(AggFunc::Count, tag("count")),
    ))
    .parse(i)
}

/// `func(lens, rel_start, rel_end)` - aggregate call usable in expressions.
fn agg_call(i: &str) -> IResult<&str, Expr> {
    let (i, func) = agg_func(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char('(')(i)?;
    let (i, _) = multispace0(i)?;
    let (i, lens) = ident(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char(',')(i)?;
    let (i, _) = multispace0(i)?;
    let (i, rel_start) = integer(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char(',')(i)?;
    let (i, _) = multispace0(i)?;
    let (i, rel_end) = integer(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char(')')(i)?;
    Ok((
        i,
        Expr::Agg {
            func,
            lens,
            rel_start,
            rel_end,
        },
    ))
}

/// Precedence climbing for expressions.  See the `expr_*` functions below
fn expr(i: &str) -> IResult<&str, Expr> {
    expr_or(i)
}

/// Logical OR (`||`) is lowest precedence, so it's the top-level `expr_*` function.
fn expr_or(i: &str) -> IResult<&str, Expr> {
    let (i, init) = expr_and(i)?;
    let (i, rest) = many0(preceded(
        delimited(multispace0, tag("||"), multispace0),
        expr_and,
    ))
    .parse(i)?;
    Ok((i, fold_left(init, rest, BinOp::Or)))
}

/// Logical AND (`&&`) is next, so it's the next-level `expr_*` function.
fn expr_and(i: &str) -> IResult<&str, Expr> {
    let (i, init) = expr_cmp(i)?;
    let (i, rest) = many0(preceded(
        delimited(multispace0, tag("&&"), multispace0),
        expr_cmp,
    ))
    .parse(i)?;
    Ok((i, fold_left(init, rest, BinOp::And)))
}

/// Comparison operators are next, and they are non-associative, so only allow one at this level.
fn expr_cmp(i: &str) -> IResult<&str, Expr> {
    let (i, lhs) = expr_sum(i)?;
    let (i, tail) = opt(pair(delimited(multispace0, cmp_op, multispace0), expr_sum)).parse(i)?;
    Ok(match tail {
        Some((op, rhs)) => (
            i,
            Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
        ),
        None => (i, lhs),
    })
}

/// Sum operators are next, and they are left-associative, so allow chaining at this level.
fn expr_sum(i: &str) -> IResult<&str, Expr> {
    let (i, init) = expr_term(i)?;
    let (i, rest) = many0(pair(delimited(multispace0, sum_op, multispace0), expr_term)).parse(i)?;
    Ok((i, fold_left_ops(init, rest)))
}

/// Term operators are next, and they are left-associative, so allow chaining at this level.
fn expr_term(i: &str) -> IResult<&str, Expr> {
    let (i, init) = expr_unary(i)?;
    let (i, rest) = many0(pair(
        delimited(multispace0, term_op, multispace0),
        expr_unary,
    ))
    .parse(i)?;
    Ok((i, fold_left_ops(init, rest)))
}

/// Unary operators are next, and they are right-associative, so only allow one at this level.
fn expr_unary(i: &str) -> IResult<&str, Expr> {
    let (i, _) = multispace0(i)?;
    alt((
        map(preceded(char('-'), expr_unary), |e| Expr::Unary {
            op: UnOp::Neg,
            expr: Box::new(e),
        }),
        map(preceded(char('!'), expr_unary), |e| Expr::Unary {
            op: UnOp::Not,
            expr: Box::new(e),
        }),
        expr_primary,
    ))
    .parse(i)
}

/// Primary expressions are literals, identifiers, agg calls, or parenthesized expressions.
fn expr_primary(i: &str) -> IResult<&str, Expr> {
    let (i, _) = multispace0(i)?;
    alt((
        delimited(
            pair(char('('), multispace0),
            expr,
            pair(multispace0, char(')')),
        ),
        // literal MUST come before ident so `true`/`false`/`null` win.
        map(literal, Expr::Lit),
        // agg_call before ident so avg(...) etc. are not parsed as bare idents.
        agg_call,
        map(ident, Expr::Ident),
    ))
    .parse(i)
}

/// Comparison operators: `==`, `!=`, `<`, `<=`, `>`, `>=`.  Note that these are non-associative, so
/// don't allow chaining at this level.
fn cmp_op(i: &str) -> IResult<&str, BinOp> {
    alt((
        value(BinOp::Eq, tag("==")),
        value(BinOp::NotEq, tag("!=")),
        value(BinOp::LtEq, tag("<=")),
        value(BinOp::GtEq, tag(">=")),
        value(BinOp::Lt, tag("<")),
        value(BinOp::Gt, tag(">")),
    ))
    .parse(i)
}

/// Sum operators: `+`, `-`.  These are left-associative, so allow chaining at this level.
fn sum_op(i: &str) -> IResult<&str, BinOp> {
    alt((value(BinOp::Add, char('+')), value(BinOp::Sub, char('-')))).parse(i)
}

/// Term operators: `*`, `/`, `%`.  These are left-associative, so allow chaining at this level.
fn term_op(i: &str) -> IResult<&str, BinOp> {
    alt((
        value(BinOp::Mul, char('*')),
        value(BinOp::Div, char('/')),
        value(BinOp::Mod, char('%')),
    ))
    .parse(i)
}

/// Helper function to fold a left-associative operator over a list of expressions.
fn fold_left(init: Expr, rest: Vec<Expr>, op: BinOp) -> Expr {
    rest.into_iter().fold(init, |acc, rhs| Expr::Binary {
        op,
        lhs: Box::new(acc),
        rhs: Box::new(rhs),
    })
}

/// Helper function to fold a list of (operator, expression) pairs over an initial expression, for
/// left-associative operators.
fn fold_left_ops(init: Expr, rest: Vec<(BinOp, Expr)>) -> Expr {
    rest.into_iter().fold(init, |acc, (op, rhs)| Expr::Binary {
        op,
        lhs: Box::new(acc),
        rhs: Box::new(rhs),
    })
}

/// A TauQL keyword followed by mandatory whitespace.  Matching is
/// case-*sensitive*: TauQL statement keywords are UPPERCASE so they are visually
/// distinct from `tauctl`'s lowercase meta-commands (`connect`, `use`, `load`).
fn kw<'a>(
    word: &'static str,
) -> impl Parser<&'a str, Output = (), Error = nom::error::Error<&'a str>> {
    map((tag(word), multispace1), |_| ())
}

/// An identifier: starts with a letter or underscore, followed by letters, digits, or underscores.
fn ident(i: &str) -> IResult<&str, String> {
    let (i, s) = recognize(pair(
        alt((alpha1, tag("_"))),
        many0(alt((alphanumeric1, tag("_")))),
    ))
    .parse(i)?;
    Ok((i, s.to_string()))
}

/// A type name: `int`, `float`, `str`, or `bool`.
fn type_name(i: &str) -> IResult<&str, Type> {
    alt((
        value(Type::Int, tag("int")),
        value(Type::Float, tag("float")),
        value(Type::Str, tag("str")),
        value(Type::Bool, tag("bool")),
    ))
    .parse(i)
}

/// An unsigned 64-bit integer literal (no leading sign).  Used for layer IDs.
fn unsigned_integer(i: &str) -> IResult<&str, u64> {
    map_res(digit1, |s: &str| s.parse::<u64>()).parse(i)
}

/// An integer literal, optionally preceded by a `-` for negative numbers.
fn integer(i: &str) -> IResult<&str, i64> {
    map_res(recognize(pair(opt(char('-')), digit1)), |s: &str| {
        s.parse::<i64>()
    })
    .parse(i)
}

/// A floating-point literal, optionally preceded by a `-` for negative numbers.  Must have a
/// decimal
fn float_lit(i: &str) -> IResult<&str, f64> {
    map_res(
        recognize((opt(char('-')), digit1, char('.'), digit1)),
        |s: &str| s.parse::<f64>(),
    )
    .parse(i)
}

/// A string literal: a sequence of characters enclosed in double quotes.  No escape sequences are
/// supported, and the string cannot contain unescaped double quotes.  An empty string is `""`.
fn string_lit(i: &str) -> IResult<&str, String> {
    delimited(
        char('"'),
        map(opt(is_not("\"")), |s: Option<&str>| {
            s.unwrap_or("").to_string()
        }),
        char('"'),
    )
    .parse(i)
}

/// A boolean literal: lowercase `true` or `false`.
fn bool_lit(i: &str) -> IResult<&str, bool> {
    alt((value(true, tag("true")), value(false, tag("false")))).parse(i)
}

/// Parse a single literal value from `s`, consuming all input.
/// Used by bulk-load paths that need to decode one scalar without
/// constructing a full statement.
pub fn parse_literal(s: &str) -> Option<Literal> {
    let (rest, lit) = literal(s.trim()).ok()?;
    if rest.trim().is_empty() {
        Some(lit)
    } else {
        None
    }
}

/// A literal value: `null`, a boolean, a string, a float, or an integer.
fn literal(i: &str) -> IResult<&str, Literal> {
    alt((
        value(Literal::Null, tag("null")),
        map(bool_lit, Literal::Bool),
        map(string_lit, |s| Literal::Str(Arc::from(s.as_str()))),
        map(float_lit, Literal::Float),
        map(integer, Literal::Int),
    ))
    .parse(i)
}

/// `BATCH APPEND LENS <name> { <s> <e> <v> [; <s> <e> <v> …] }` -
/// block-syntax bulk ingest.  Taus are separated by `;` (plus optional
/// surrounding whitespace).  The block may span multiple lines when the
/// caller has already joined them with a space.
fn stmt_batch_append(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("BATCH").parse(i)?;
    let (i, _) = kw("APPEND").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char('{')(i)?;
    let (i, _) = multispace0(i)?;
    // Taus inside the block are separated by optional whitespace + ';' +
    // optional whitespace.  An empty block `{}` yields zero taus.
    let (i, taus) =
        separated_list0(delimited(multispace0, char(';'), multispace0), tau_triple).parse(i)?;
    let (i, _) = multispace0(i)?;
    // Allow a trailing semicolon before `}`.
    let (i, _) = opt(char(';')).parse(i)?;
    let (i, _) = multispace0(i)?;
    let (i, _) = char('}')(i)?;
    Ok((i, Stmt::BatchAppend { name, taus }))
}

/// `HISTORY LENS <name> [<start> <end>]` - list layers (optionally filtered
/// to those that overlap `[start, end)`).
fn stmt_history(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("HISTORY").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, range) = opt(pair(
        preceded(multispace1, integer),
        preceded(multispace1, integer),
    ))
    .parse(i)?;
    Ok((i, Stmt::HistoryLens { name, range }))
}

/// `BACKUP DATABASE <name> TO "<path>"` - snapshot to a file.
fn stmt_backup(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("BACKUP").parse(i)?;
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, path) = path_suffix(i, "TO")?;
    Ok((i, Stmt::BackupDatabase { name, path }))
}

/// `RESTORE DATABASE <name> FROM "<path>"` - replay a backup snapshot.
fn stmt_restore(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("RESTORE").parse(i)?;
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, path) = path_suffix(i, "FROM")?;
    Ok((i, Stmt::RestoreDatabase { name, path }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    fn parsed(input: &str) -> Stmt {
        let (rest, s) = parse(input).expect("parse failed");
        assert!(
            rest.is_empty(),
            "unconsumed input: {rest:?} for query {input:?}"
        );
        s
    }

    fn ident_gen() -> impl Generator<String> {
        gs::from_regex("[a-z][a-z0-9_]{0,10}").fullmatch(true)
    }

    fn type_keyword_gen() -> impl Generator<(&'static str, Type)> {
        gs::sampled_from(vec![
            ("int", Type::Int),
            ("float", Type::Float),
            ("str", Type::Str),
            ("bool", Type::Bool),
        ])
    }

    fn agg_keyword_gen() -> impl Generator<(&'static str, AggFunc)> {
        gs::sampled_from(vec![
            ("min", AggFunc::Min),
            ("max", AggFunc::Max),
            ("avg", AggFunc::Avg),
            ("sum", AggFunc::Sum),
            ("count", AggFunc::Count),
        ])
    }

    #[hegel::test]
    fn pbt_parse_never_panics_on_arbitrary_input(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(256));
        // The parser must return an `IResult` for every possible input - no
        // panics, no infinite loops, no allocation explosions.
        let _ = parse(&s);
    }

    #[hegel::test]
    fn pbt_create_database_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        assert_eq!(
            parsed(&format!("CREATE DATABASE {name}")),
            Stmt::CreateDatabase { name }
        );
    }

    #[hegel::test]
    fn pbt_drop_database_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        assert_eq!(
            parsed(&format!("DROP DATABASE {name}")),
            Stmt::DropDatabase { name }
        );
    }

    #[hegel::test]
    fn pbt_use_database_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        assert_eq!(
            parsed(&format!("USE DATABASE {name}")),
            Stmt::UseDatabase { name }
        );
    }

    #[hegel::test]
    fn pbt_create_lens_roundtrips_for_every_type(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let (kw, ty) = tc.draw(type_keyword_gen());
        assert_eq!(
            parsed(&format!("CREATE LENS {name} {kw}")),
            Stmt::Create {
                name,
                ty,
                axes: vec![]
            }
        );
    }

    #[hegel::test]
    fn pbt_drop_lens_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        assert_eq!(parsed(&format!("DROP LENS {name}")), Stmt::Drop { name });
    }

    #[hegel::test]
    fn pbt_at_lens_roundtrips_with_any_timestamp(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let t = tc.draw(gs::integers::<i64>());
        assert_eq!(parsed(&format!("AT LENS {name} {t}")), Stmt::At { name, t });
    }

    #[hegel::test]
    fn pbt_range_lens_roundtrips_without_filter(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let start = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let end = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        assert_eq!(
            parsed(&format!("RANGE LENS {name} {start} {end}")),
            Stmt::Range {
                name,
                start,
                end,
                filter: None,
                limit: None,
                offset: None,
            }
        );
    }

    #[hegel::test]
    fn pbt_reduce_roundtrips_for_every_func(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let start = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let end = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let (kw, func) = tc.draw(agg_keyword_gen());
        assert_eq!(
            parsed(&format!("REDUCE LENS {name} {start} {end} USING {kw}")),
            Stmt::Reduce {
                name,
                start,
                end,
                func
            }
        );
    }

    #[hegel::test]
    fn pbt_append_lens_int_roundtrips(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let s = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let e = tc.draw(
            gs::integers::<i64>()
                .min_value(-1_000_000)
                .max_value(1_000_000),
        );
        let v = tc.draw(gs::integers::<i64>());
        assert_eq!(
            parsed(&format!("APPEND LENS {name} {s} {e} {v}")),
            Stmt::Append {
                name,
                taus: vec![(s, e, Literal::Int(v))]
            }
        );
    }

    #[hegel::test]
    fn pbt_append_lens_bool_and_null_roundtrip(tc: TestCase) {
        let name = tc.draw(ident_gen());
        let b = tc.draw(gs::booleans());
        let null_or_bool = tc.draw(gs::booleans());
        let (literal_str, literal) = if null_or_bool {
            (b.to_string(), Literal::Bool(b))
        } else {
            ("null".to_string(), Literal::Null)
        };
        assert_eq!(
            parsed(&format!("APPEND LENS {name} 0 5 {literal_str}")),
            Stmt::Append {
                name,
                taus: vec![(0, 5, literal)],
            }
        );
    }

    #[hegel::test]
    fn pbt_keywords_are_case_sensitive_uppercase(tc: TestCase) {
        let name = tc.draw(ident_gen());
        // TauQL statement keywords are UPPERCASE-only so they never collide with
        // tauctl's lowercase meta-commands.  The uppercase form parses; the
        // all-lowercase form (keyword `create lens`) must be rejected.
        let upper = format!("CREATE LENS {name} int");
        assert_eq!(
            parsed(&upper),
            Stmt::Create {
                name: name.clone(),
                ty: Type::Int,
                axes: vec![],
            }
        );
        let lower = upper.to_lowercase();
        assert!(
            parse(&lower).is_err(),
            "lowercase keywords must be rejected: {lower:?}"
        );
    }

    #[hegel::test]
    fn pbt_extra_whitespace_is_tolerated(tc: TestCase) {
        let pad_a = " ".repeat(tc.draw(gs::integers::<usize>().min_value(0).max_value(8)));
        let pad_b = " ".repeat(tc.draw(gs::integers::<usize>().min_value(1).max_value(8)));
        let pad_c = " ".repeat(tc.draw(gs::integers::<usize>().min_value(1).max_value(8)));
        let pad_d = " ".repeat(tc.draw(gs::integers::<usize>().min_value(0).max_value(8)));
        let q = format!("{pad_a}CREATE{pad_b}LENS{pad_b}x{pad_c}int{pad_d}");
        assert_eq!(
            parsed(&q),
            Stmt::Create {
                name: "x".into(),
                ty: Type::Int,
                axes: vec![],
            }
        );
    }

    #[hegel::test]
    fn pbt_parse_rejects_unknown_leading_token(tc: TestCase) {
        let junk = tc.draw(gs::from_regex("[A-Z]{3,8}").fullmatch(true).filter(|s| {
            !matches!(
                s.as_str(),
                "CREATE"
                    | "DROP"
                    | "USE"
                    | "APPEND"
                    | "BATCH"
                    | "COPY"
                    | "DERIVE"
                    | "XDERIVE"
                    | "SHOW"
                    | "AT"
                    | "RANGE"
                    | "REDUCE"
                    | "GRANT"
                    | "REVOKE"
                    | "COMMIT"
                    | "ROLLBACK"
                    | "HISTORY"
                    | "BACKUP"
                    | "RESTORE"
            )
        }));
        assert!(parse(&format!("{junk} LENS x 1")).is_err());
    }

    /// Show statements: examples retained because they have no parameters.
    #[test]
    fn show_statements_parse() {
        assert_eq!(parsed("SHOW DATABASES"), Stmt::ShowDatabases);
        assert_eq!(parsed("SHOW LENSES"), Stmt::ShowLenses);
        assert_eq!(parsed("SHOW USERS"), Stmt::ShowUsers);
        assert_eq!(parsed("SHOW GRANTS"), Stmt::ShowGrants { user: None });
        assert_eq!(
            parsed("SHOW GRANTS alice"),
            Stmt::ShowGrants {
                user: Some("alice".into())
            }
        );
    }

    /// Regression anchors for complex expression parsing.  PBT-style generation
    /// of arbitrary `Expr` round-trips is doable but the value-to-effort ratio
    /// drops sharply once you account for operator precedence in Display.
    #[test]
    fn expr_precedence_and_parens() {
        let (_, e1) = expr("1 + 2 * 3").unwrap();
        assert!(matches!(e1, Expr::Binary { op: BinOp::Add, .. }));
        let (_, e2) = expr("(1 + 2) * 3").unwrap();
        assert!(matches!(e2, Expr::Binary { op: BinOp::Mul, .. }));
    }

    #[test]
    fn unary_neg_and_not() {
        let (_, e1) = expr("-x").unwrap();
        assert!(matches!(e1, Expr::Unary { op: UnOp::Neg, .. }));
        let (_, e2) = expr("!flag").unwrap();
        assert!(matches!(e2, Expr::Unary { op: UnOp::Not, .. }));
    }

    #[test]
    fn append_lens_multi_tau() {
        assert_eq!(
            parsed("APPEND LENS x 0 5 1, 5 10 2, 10 15 3"),
            Stmt::Append {
                name: "x".into(),
                taus: vec![
                    (0, 5, Literal::Int(1)),
                    (5, 10, Literal::Int(2)),
                    (10, 15, Literal::Int(3)),
                ],
            }
        );
    }

    #[test]
    fn create_lens_axes_clause() {
        assert_eq!(
            parsed("CREATE LENS grid int AXES (valid, region)"),
            Stmt::Create {
                name: "grid".into(),
                ty: Type::Int,
                axes: vec!["valid".into(), "region".into()],
            }
        );
        // Display of the parsed statement must re-parse identically (WAL replay).
        let stmt = parsed("CREATE LENS grid int AXES (valid, region)");
        assert_eq!(parsed(&stmt.to_string()), stmt);
    }

    #[test]
    fn append_lens_nd_boxes() {
        assert_eq!(
            parsed("APPEND LENS grid [0 10] [100 200] 42, [0 10] [200 300] 7"),
            Stmt::AppendNd {
                name: "grid".into(),
                taus: vec![
                    (vec![(0, 10), (100, 200)], Literal::Int(42)),
                    (vec![(0, 10), (200, 300)], Literal::Int(7)),
                ],
            }
        );
    }

    #[test]
    fn at_lens_nd_coords_with_optional_as_of() {
        assert_eq!(
            parsed("AT LENS grid 5 150"),
            Stmt::AtNd {
                name: "grid".into(),
                ts: vec![5, 150],
                as_of: None,
            }
        );
        assert_eq!(
            parsed("AT LENS grid 5 150 AS OF 1700000000000"),
            Stmt::AtNd {
                name: "grid".into(),
                ts: vec![5, 150],
                as_of: Some(1_700_000_000_000),
            }
        );
        // A single coordinate stays the classic 1-D statement.
        assert_eq!(
            parsed("AT LENS grid 5"),
            Stmt::At {
                name: "grid".into(),
                t: 5,
            }
        );
    }

    #[test]
    fn range_lens_nd_fixed_points() {
        assert_eq!(
            parsed("RANGE LENS grid 0 100 AT (150)"),
            Stmt::RangeNd {
                name: "grid".into(),
                start: 0,
                end: 100,
                fixed: vec![150],
            }
        );
        assert_eq!(
            parsed("RANGE LENS cube -5 50 AT (1, -2)"),
            Stmt::RangeNd {
                name: "cube".into(),
                start: -5,
                end: 50,
                fixed: vec![1, -2],
            }
        );
    }

    #[test]
    fn append_lens_string_with_spaces() {
        assert_eq!(
            parsed("APPEND LENS msg 0 10 \"hello world\""),
            Stmt::Append {
                name: "msg".into(),
                taus: vec![(0, 10, Literal::Str("hello world".into()))],
            }
        );
    }

    #[test]
    fn copy_lens_parses() {
        assert_eq!(
            parsed("COPY LENS temp FROM \"/data/temps.csv\""),
            Stmt::Copy {
                name: "temp".into(),
                path: "/data/temps.csv".into(),
            }
        );
    }

    #[test]
    fn derive_with_agg_call_and_arithmetic() {
        let stmt = parsed("DERIVE LENS hot AS x > avg(x, -10, 0)");
        let Stmt::Derive { name, expr, range } = stmt else {
            panic!()
        };
        assert_eq!(name, "hot");
        assert_eq!(range, None);
        let Expr::Binary { op, .. } = expr else {
            panic!()
        };
        assert_eq!(op, BinOp::Gt);
    }

    #[test]
    fn xderive_parses_with_and_without_over() {
        let stmt = parsed("XDERIVE LENS doubled AS c * 2");
        assert_eq!(
            stmt,
            Stmt::Xderive {
                name: "doubled".into(),
                expr: Expr::Binary {
                    op: BinOp::Mul,
                    lhs: Box::new(Expr::Ident("c".into())),
                    rhs: Box::new(Expr::Lit(Literal::Int(2))),
                },
                range: None,
            }
        );
        let stmt = parsed("XDERIVE LENS w AS c OVER 0 100");
        let Stmt::Xderive { name, range, .. } = stmt else {
            panic!()
        };
        assert_eq!(name, "w");
        assert_eq!(range, Some((0, 100)));
    }

    #[test]
    fn format_parse_error_is_human_readable() {
        // A typo'd keyword (the failure mode behind the original XDERIVE bug
        // report, where an old server didn't know the keyword) must produce a
        // friendly column-anchored message, never nom's `code: Tag` debug dump.
        let q = "XDERIV LENS x AS y * 3";
        let msg = format_parse_error(q, parse(q).unwrap_err());
        assert!(msg.starts_with("parse error at column"), "{msg}");
        assert!(!msg.contains("code:"), "{msg}");
    }

    #[test]
    fn derive_with_over_clause_parses() {
        let stmt = parsed("DERIVE LENS d AS c OVER -5 10");
        let Stmt::Derive { range, .. } = stmt else {
            panic!()
        };
        assert_eq!(range, Some((-5, 10)));
    }

    #[test]
    fn range_with_limit_parses() {
        assert_eq!(
            parsed("RANGE LENS x 0 100 LIMIT 10"),
            Stmt::Range {
                name: "x".into(),
                start: 0,
                end: 100,
                filter: None,
                limit: Some(10),
                offset: None,
            }
        );
        assert_eq!(
            parsed("RANGE LENS x 0 100 WHERE x > 5 LIMIT 3"),
            Stmt::Range {
                name: "x".into(),
                start: 0,
                end: 100,
                filter: Some(Expr::Binary {
                    op: BinOp::Gt,
                    lhs: Box::new(Expr::Ident("x".into())),
                    rhs: Box::new(Expr::Lit(Literal::Int(5))),
                }),
                limit: Some(3),
                offset: None,
            }
        );
    }

    #[test]
    fn range_with_compound_where_clause() {
        let stmt = parsed("RANGE LENS x 0 100 WHERE x > 5 && x < 50");
        let Stmt::Range {
            filter: Some(Expr::Binary { op, .. }),
            ..
        } = stmt
        else {
            panic!()
        };
        assert_eq!(op, BinOp::And);
    }

    #[test]
    fn start_transaction_parses() {
        assert_eq!(parsed("START TRANSACTION"), Stmt::StartTransaction);
        // Lowercase keywords are not TauQL (they are reserved for tauctl
        // meta-commands), so they must not parse.
        assert!(parse("start transaction").is_err());
    }

    #[test]
    fn commit_parses() {
        assert_eq!(parsed("COMMIT"), Stmt::Commit);
        assert!(parse("commit").is_err());
    }

    #[test]
    fn rollback_parses() {
        assert_eq!(parsed("ROLLBACK"), Stmt::Rollback);
        assert!(parse("rollback").is_err());
    }

    #[test]
    fn malformed_inputs_fail() {
        assert!(parse("CREATE test int").is_err()); // missing LENS
        assert!(parse("APPEND LENS test 0").is_err()); // truncated tau
    }

    #[test]
    fn lowercase_vocabulary_is_locked_lowercase() {
        // Type names, aggregate functions and value literals are the lowercase
        // exceptions to the UPPERCASE-keyword rule; their uppercase forms are
        // not valid TauQL.
        assert!(parse("CREATE LENS x int").is_ok());
        assert!(
            parse("CREATE LENS x INT").is_err(),
            "uppercase type rejected"
        );
        assert!(parse("REDUCE LENS x 0 10 USING avg").is_ok());
        assert!(
            parse("REDUCE LENS x 0 10 USING AVG").is_err(),
            "uppercase agg func rejected"
        );
        assert!(parse("APPEND LENS f 0 10 true").is_ok());
        assert!(
            parse("APPEND LENS f 0 10 TRUE").is_err(),
            "uppercase bool literal rejected"
        );
    }

    #[test]
    fn reduce_is_read_only() {
        assert!(
            Stmt::Reduce {
                name: "x".into(),
                start: 0,
                end: 10,
                func: AggFunc::Min
            }
            .is_read_only()
        );
    }
}
