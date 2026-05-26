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
    bytes::complete::{is_not, tag, tag_no_case, take_while1},
    character::complete::{alpha1, alphanumeric1, char, digit1, multispace0, multispace1},
    combinator::{map, map_res, opt, recognize, value},
    multi::many0,
    sequence::{delimited, pair, preceded},
};

use super::ast::*;
use crate::libtau::users::Perm;

/// Parse a single statement.  Trailing whitespace is consumed but trailing
/// crap is reported as an error.
pub fn parse(input: &str) -> IResult<&str, Stmt> {
    let (input, _) = multispace0(input)?;
    let (input, s) = alt((
        stmt_create,
        stmt_append,
        stmt_copy,
        stmt_derive,
        stmt_at,
        stmt_range,
        stmt_reduce,
        stmt_drop,
        stmt_use,
        stmt_show,
        stmt_grant,
        stmt_revoke,
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
    let (i, _) = tag_no_case("PASSWORD")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, password) = string_lit(i)?;
    Ok((i, Stmt::CreateUser { name, password }))
}

fn stmt_create_lens(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, ty) = type_name(i)?;
    Ok((i, Stmt::Create { name, ty }))
}

fn stmt_create_database(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::CreateDatabase { name }))
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

fn stmt_append(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("APPEND").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
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

/// `COPY LENS <name> FROM "<path>"` - bulk-ingest from a CSV file.
fn stmt_copy(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("COPY").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag_no_case("FROM")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, path) = string_lit(i)?;
    Ok((i, Stmt::Copy { name, path }))
}

/// `SHOW DATABASES`, `SHOW LENSES`, `SHOW USERS`, or `SHOW GRANTS [<name>]`.
fn stmt_show(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("SHOW").parse(i)?;
    alt((
        value(Stmt::ShowDatabases, tag_no_case("DATABASES")),
        value(Stmt::ShowLenses, tag_no_case("LENSES")),
        value(Stmt::ShowUsers, tag_no_case("USERS")),
        stmt_show_grants,
    ))
    .parse(i)
}

fn stmt_show_grants(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = tag_no_case("GRANTS")(i)?;
    let (i, user) = opt(preceded(multispace1, ident)).parse(i)?;
    Ok((i, Stmt::ShowGrants { user }))
}

fn stmt_derive(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DERIVE").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag_no_case("AS")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, e) = expr(i)?;
    Ok((i, Stmt::Derive { name, expr: e }))
}

fn stmt_at(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("AT").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, t) = integer(i)?;
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
    let (i, filter) = opt(preceded(
        (multispace1, tag_no_case("WHERE"), multispace1),
        expr,
    ))
    .parse(i)?;
    Ok((
        i,
        Stmt::Range {
            name,
            start,
            end,
            filter,
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

/// `GRANT <perms> ON <db|*> TO <user>`.
fn stmt_grant(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("GRANT").parse(i)?;
    let (i, perms) = perm_letters(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag_no_case("ON")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, database) = db_target(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag_no_case("TO")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, user) = ident(i)?;
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
    let (i, perms) = perm_letters(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag_no_case("ON")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, database) = db_target(i)?;
    let (i, _) = multispace1(i)?;
    let (i, _) = tag_no_case("FROM")(i)?;
    let (i, _) = multispace1(i)?;
    let (i, user) = ident(i)?;
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
    let (i, _) = tag_no_case("USING")(i)?;
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

/// `USE DATABASE <name>` - switches the active database.
fn stmt_use(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("USE").parse(i)?;
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::UseDatabase { name }))
}

fn agg_func(i: &str) -> IResult<&str, AggFunc> {
    alt((
        value(AggFunc::Min, tag_no_case("min")),
        value(AggFunc::Max, tag_no_case("max")),
        value(AggFunc::Avg, tag_no_case("avg")),
        value(AggFunc::Sum, tag_no_case("sum")),
        value(AggFunc::Count, tag_no_case("count")),
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

/// A case-insensitive keyword followed by mandatory whitespace.
fn kw<'a>(
    word: &'static str,
) -> impl Parser<&'a str, Output = (), Error = nom::error::Error<&'a str>> {
    map((tag_no_case(word), multispace1), |_| ())
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

/// A type name: `int`, `float`, `str`, `bool`, or `bytes`.
fn type_name(i: &str) -> IResult<&str, Type> {
    alt((
        value(Type::Int, tag_no_case("int")),
        value(Type::Float, tag_no_case("float")),
        value(Type::Str, tag_no_case("str")),
        value(Type::Bool, tag_no_case("bool")),
        value(Type::Bytes, tag_no_case("bytes")),
    ))
    .parse(i)
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

/// A boolean literal: `true` or `false`, case-insensitive.
fn bool_lit(i: &str) -> IResult<&str, bool> {
    alt((
        value(true, tag_no_case("true")),
        value(false, tag_no_case("false")),
    ))
    .parse(i)
}

/// A literal value: `null`, a boolean, a string, a float, or an integer.
fn literal(i: &str) -> IResult<&str, Literal> {
    alt((
        value(Literal::Null, tag_no_case("null")),
        map(bool_lit, Literal::Bool),
        map(string_lit, |s| Literal::Str(Arc::from(s.as_str()))),
        map(float_lit, Literal::Float),
        map(integer, Literal::Int),
    ))
    .parse(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok<T: PartialEq + std::fmt::Debug>(res: IResult<&str, T>, want: T, rest: &str) {
        let (left, got) = res.expect("parse failed");
        assert_eq!(got, want);
        assert_eq!(left, rest);
    }

    fn parsed(input: &str) -> Stmt {
        let (rest, s) = parse(input).expect("parse failed");
        assert!(
            rest.is_empty(),
            "unconsumed input: {rest:?} for query {input:?}"
        );
        s
    }

    #[test]
    fn create_lens_int() {
        assert_eq!(
            parsed("CREATE LENS test int"),
            Stmt::Create {
                name: "test".into(),
                ty: Type::Int,
            }
        );
    }

    #[test]
    fn create_lens_all_types() {
        for (s, ty) in [
            ("int", Type::Int),
            ("float", Type::Float),
            ("str", Type::Str),
            ("bool", Type::Bool),
            ("bytes", Type::Bytes),
        ] {
            assert_eq!(
                parsed(&format!("CREATE LENS x {s}")),
                Stmt::Create {
                    name: "x".into(),
                    ty,
                }
            );
        }
    }

    #[test]
    fn append_lens_int() {
        assert_eq!(
            parsed("APPEND LENS test 0 10 42"),
            Stmt::Append {
                name: "test".into(),
                taus: vec![(0, 10, Literal::Int(42))],
            }
        );
    }

    #[test]
    fn append_lens_float() {
        assert_eq!(
            parsed("APPEND LENS temp 0 10 18.5"),
            Stmt::Append {
                name: "temp".into(),
                taus: vec![(0, 10, Literal::Float(18.5))],
            }
        );
    }

    #[test]
    fn append_lens_string() {
        assert_eq!(
            parsed("APPEND LENS msg 0 10 \"hello world\""),
            Stmt::Append {
                name: "msg".into(),
                taus: vec![(0, 10, Literal::Str("hello world".into()))],
            }
        );
    }

    #[test]
    fn append_lens_bool_and_null() {
        assert_eq!(
            parsed("APPEND LENS f 0 5 true"),
            Stmt::Append {
                name: "f".into(),
                taus: vec![(0, 5, Literal::Bool(true))],
            }
        );
        assert_eq!(
            parsed("APPEND LENS f 0 5 null"),
            Stmt::Append {
                name: "f".into(),
                taus: vec![(0, 5, Literal::Null)],
            }
        );
    }

    #[test]
    fn append_lens_negative_range() {
        assert_eq!(
            parsed("APPEND LENS x -10 -5 -1"),
            Stmt::Append {
                name: "x".into(),
                taus: vec![(-10, -5, Literal::Int(-1))],
            }
        );
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
    fn show_databases_parses() {
        assert_eq!(parsed("SHOW DATABASES"), Stmt::ShowDatabases);
    }

    #[test]
    fn show_lenses_parses() {
        assert_eq!(parsed("SHOW LENSES"), Stmt::ShowLenses);
    }

    #[test]
    fn show_is_case_insensitive() {
        assert_eq!(parsed("show databases"), Stmt::ShowDatabases);
        assert_eq!(parsed("SHOW lenses"), Stmt::ShowLenses);
    }

    #[test]
    fn derive_lens_simple_expression() {
        assert_eq!(
            parsed("DERIVE LENS test2 AS test * 2"),
            Stmt::Derive {
                name: "test2".into(),
                expr: Expr::Binary {
                    op: BinOp::Mul,
                    lhs: Box::new(Expr::Ident("test".into())),
                    rhs: Box::new(Expr::Lit(Literal::Int(2))),
                },
            }
        );
    }

    #[test]
    fn derive_lens_no_whitespace_around_star() {
        // matches the original spec exactly: `test*2`
        assert_eq!(
            parsed("DERIVE LENS test2 AS test*2"),
            Stmt::Derive {
                name: "test2".into(),
                expr: Expr::Binary {
                    op: BinOp::Mul,
                    lhs: Box::new(Expr::Ident("test".into())),
                    rhs: Box::new(Expr::Lit(Literal::Int(2))),
                },
            }
        );
    }

    #[test]
    fn derive_lens_celsius_to_fahrenheit() {
        let Stmt::Derive { name, expr } = parsed("DERIVE LENS f AS celsius * 9 / 5 + 32") else {
            panic!("expected derive");
        };
        assert_eq!(name, "f");
        // ((celsius * 9) / 5) + 32
        assert_eq!(
            expr,
            Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Binary {
                    op: BinOp::Div,
                    lhs: Box::new(Expr::Binary {
                        op: BinOp::Mul,
                        lhs: Box::new(Expr::Ident("celsius".into())),
                        rhs: Box::new(Expr::Lit(Literal::Int(9))),
                    }),
                    rhs: Box::new(Expr::Lit(Literal::Int(5))),
                }),
                rhs: Box::new(Expr::Lit(Literal::Int(32))),
            }
        );
    }

    #[test]
    fn at_lens_point_lookup() {
        assert_eq!(
            parsed("AT LENS test 1"),
            Stmt::At {
                name: "test".into(),
                t: 1,
            }
        );
    }

    #[test]
    fn range_lens_bounds_only() {
        assert_eq!(
            parsed("RANGE LENS test 0 10"),
            Stmt::Range {
                name: "test".into(),
                start: 0,
                end: 10,
                filter: None,
            }
        );
    }

    #[test]
    fn range_lens_with_where_clause() {
        assert_eq!(
            parsed("RANGE LENS test 0 100 WHERE test > 5"),
            Stmt::Range {
                name: "test".into(),
                start: 0,
                end: 100,
                filter: Some(Expr::Binary {
                    op: BinOp::Gt,
                    lhs: Box::new(Expr::Ident("test".into())),
                    rhs: Box::new(Expr::Lit(Literal::Int(5))),
                }),
            }
        );
    }

    #[test]
    fn range_lens_with_compound_predicate() {
        let Stmt::Range { filter, .. } = parsed("RANGE LENS x 0 100 WHERE x > 5 && x < 50") else {
            panic!("expected range");
        };
        let want = Expr::Binary {
            op: BinOp::And,
            lhs: Box::new(Expr::Binary {
                op: BinOp::Gt,
                lhs: Box::new(Expr::Ident("x".into())),
                rhs: Box::new(Expr::Lit(Literal::Int(5))),
            }),
            rhs: Box::new(Expr::Binary {
                op: BinOp::Lt,
                lhs: Box::new(Expr::Ident("x".into())),
                rhs: Box::new(Expr::Lit(Literal::Int(50))),
            }),
        };
        assert_eq!(filter, Some(want));
    }

    #[test]
    fn drop_lens() {
        assert_eq!(
            parsed("DROP LENS test"),
            Stmt::Drop {
                name: "test".into(),
            }
        );
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            parsed("create lens test int"),
            Stmt::Create {
                name: "test".into(),
                ty: Type::Int,
            }
        );
        assert_eq!(
            parsed("At Lens test 7"),
            Stmt::At {
                name: "test".into(),
                t: 7,
            }
        );
    }

    #[test]
    fn extra_whitespace_is_tolerated() {
        assert_eq!(
            parsed("   CREATE   LENS   test   int   "),
            Stmt::Create {
                name: "test".into(),
                ty: Type::Int,
            }
        );
    }

    #[test]
    fn missing_lens_keyword_fails() {
        assert!(parse("CREATE test int").is_err());
    }

    #[test]
    fn unknown_statement_fails() {
        assert!(parse("FOO LENS test 1").is_err());
    }

    #[test]
    fn malformed_append_fails() {
        // missing end + value
        assert!(parse("APPEND LENS test 0").is_err());
    }

    #[test]
    fn ident_accepts_underscores_and_digits() {
        ok(ident("sensor_1"), "sensor_1".into(), "");
        ok(ident("_x"), "_x".into(), "");
    }

    #[test]
    fn integer_handles_signs() {
        ok(integer("42"), 42, "");
        ok(integer("-7"), -7, "");
    }

    #[test]
    fn string_lit_handles_empty() {
        ok(string_lit("\"\""), "".into(), "");
    }

    #[test]
    fn literal_precedence_keywords_over_ident() {
        ok(literal("true"), Literal::Bool(true), "");
        ok(literal("null"), Literal::Null, "");
    }

    #[test]
    fn expr_precedence_mul_before_add() {
        let (_, e) = expr("1 + 2 * 3").unwrap();
        assert_eq!(
            e,
            Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Lit(Literal::Int(1))),
                rhs: Box::new(Expr::Binary {
                    op: BinOp::Mul,
                    lhs: Box::new(Expr::Lit(Literal::Int(2))),
                    rhs: Box::new(Expr::Lit(Literal::Int(3))),
                }),
            }
        );
    }

    #[test]
    fn expr_parens_override_precedence() {
        let (_, e) = expr("(1 + 2) * 3").unwrap();
        assert_eq!(
            e,
            Expr::Binary {
                op: BinOp::Mul,
                lhs: Box::new(Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Lit(Literal::Int(1))),
                    rhs: Box::new(Expr::Lit(Literal::Int(2))),
                }),
                rhs: Box::new(Expr::Lit(Literal::Int(3))),
            }
        );
    }

    #[test]
    fn expr_unary_neg_and_not() {
        let (_, e) = expr("-x").unwrap();
        assert_eq!(
            e,
            Expr::Unary {
                op: UnOp::Neg,
                expr: Box::new(Expr::Ident("x".into())),
            }
        );
        let (_, e) = expr("!flag").unwrap();
        assert_eq!(
            e,
            Expr::Unary {
                op: UnOp::Not,
                expr: Box::new(Expr::Ident("flag".into())),
            }
        );
    }

    #[test]
    fn reduce_lens_parses() {
        assert_eq!(
            parsed("REDUCE LENS temp 0 100 USING avg"),
            Stmt::Reduce {
                name: "temp".into(),
                start: 0,
                end: 100,
                func: AggFunc::Avg,
            }
        );
    }

    #[test]
    fn reduce_all_agg_funcs() {
        for (s, func) in [
            ("min", AggFunc::Min),
            ("max", AggFunc::Max),
            ("avg", AggFunc::Avg),
            ("sum", AggFunc::Sum),
            ("count", AggFunc::Count),
        ] {
            assert_eq!(
                parsed(&format!("REDUCE LENS x 0 10 USING {s}")),
                Stmt::Reduce {
                    name: "x".into(),
                    start: 0,
                    end: 10,
                    func
                }
            );
        }
    }

    #[test]
    fn agg_call_in_expr() {
        let (_, e) = expr("avg(temp, -10, 0)").unwrap();
        assert_eq!(
            e,
            Expr::Agg {
                func: AggFunc::Avg,
                lens: "temp".into(),
                rel_start: -10,
                rel_end: 0,
            }
        );
    }

    #[test]
    fn agg_call_in_derive() {
        assert_eq!(
            parsed("DERIVE LENS smooth AS avg(temp, -10, 0)"),
            Stmt::Derive {
                name: "smooth".into(),
                expr: Expr::Agg {
                    func: AggFunc::Avg,
                    lens: "temp".into(),
                    rel_start: -10,
                    rel_end: 0,
                },
            }
        );
    }

    #[test]
    fn agg_call_composable_in_binary_expr() {
        // x > avg(x, -10, 0)
        let (_, e) = expr("x > avg(x, -10, 0)").unwrap();
        assert_eq!(
            e,
            Expr::Binary {
                op: BinOp::Gt,
                lhs: Box::new(Expr::Ident("x".into())),
                rhs: Box::new(Expr::Agg {
                    func: AggFunc::Avg,
                    lens: "x".into(),
                    rel_start: -10,
                    rel_end: 0,
                }),
            }
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
