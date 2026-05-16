//! `nom`-based parser for the Tau query language.
//!
//! Entry point is [`parse`], which returns a single [`Stmt`].  Operator
//! precedence (lowest → highest) is:
//!
//! 1. `||`
//! 2. `&&`
//! 3. `==`, `!=`, `<`, `<=`, `>`, `>=`
//! 4. `+`, `-`
//! 5. `*`, `/`, `%`
//! 6. unary `-`, `!`
//! 7. literals, identifiers, `(` expr `)`

use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{is_not, tag, tag_no_case},
    character::complete::{alpha1, alphanumeric1, char, digit1, multispace0, multispace1},
    combinator::{map, map_res, opt, recognize, value},
    multi::many0,
    sequence::{delimited, pair, preceded},
};

use super::ast::*;

/// Parse a single statement.  Trailing whitespace is consumed but trailing
/// crap is reported as an error.
pub fn parse(input: &str) -> IResult<&str, Stmt> {
    let (input, _) = multispace0(input)?;
    let (input, s) = alt((
        stmt_create,
        stmt_append,
        stmt_derive,
        stmt_at,
        stmt_range,
        stmt_drop,
        stmt_use,
    ))
    .parse(input)?;
    let (input, _) = multispace0(input)?;
    Ok((input, s))
}

// `CREATE LENS <name> <type>` or `CREATE DATABASE <name>`.
fn stmt_create(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("CREATE").parse(i)?;
    alt((stmt_create_lens, stmt_create_database)).parse(i)
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

/// Append statement for lens
fn stmt_append(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("APPEND").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, start) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, end) = integer(i)?;
    let (i, _) = multispace1(i)?;
    let (i, value) = literal(i)?;
    Ok((
        i,
        Stmt::Append {
            name,
            start,
            end,
            value,
        },
    ))
}

/// Derive statement for lens
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

/// At statement for lens
fn stmt_at(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("AT").parse(i)?;
    let (i, _) = kw("LENS").parse(i)?;
    let (i, name) = ident(i)?;
    let (i, _) = multispace1(i)?;
    let (i, t) = integer(i)?;
    Ok((i, Stmt::At { name, t }))
}

/// Range statement for lens, with optional WHERE clause
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

/// `DROP LENS <name>` or `DROP DATABASE <name>`.
fn stmt_drop(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("DROP").parse(i)?;
    alt((stmt_drop_lens, stmt_drop_database)).parse(i)
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

/// `USE DATABASE <name>` — switches the active database.
fn stmt_use(i: &str) -> IResult<&str, Stmt> {
    let (i, _) = kw("USE").parse(i)?;
    let (i, _) = kw("DATABASE").parse(i)?;
    let (i, name) = ident(i)?;
    Ok((i, Stmt::UseDatabase { name }))
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

/// Primary expressions are literals, identifiers, or parenthesized expressions.
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
        map(string_lit, Literal::Str),
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
                start: 0,
                end: 10,
                value: Literal::Int(42),
            }
        );
    }

    #[test]
    fn append_lens_float() {
        assert_eq!(
            parsed("APPEND LENS temp 0 10 18.5"),
            Stmt::Append {
                name: "temp".into(),
                start: 0,
                end: 10,
                value: Literal::Float(18.5),
            }
        );
    }

    #[test]
    fn append_lens_string() {
        assert_eq!(
            parsed("APPEND LENS msg 0 10 \"hello world\""),
            Stmt::Append {
                name: "msg".into(),
                start: 0,
                end: 10,
                value: Literal::Str("hello world".into()),
            }
        );
    }

    #[test]
    fn append_lens_bool_and_null() {
        assert_eq!(
            parsed("APPEND LENS f 0 5 true"),
            Stmt::Append {
                name: "f".into(),
                start: 0,
                end: 5,
                value: Literal::Bool(true),
            }
        );
        assert_eq!(
            parsed("APPEND LENS f 0 5 null"),
            Stmt::Append {
                name: "f".into(),
                start: 0,
                end: 5,
                value: Literal::Null,
            }
        );
    }

    #[test]
    fn append_lens_negative_range() {
        assert_eq!(
            parsed("APPEND LENS x -10 -5 -1"),
            Stmt::Append {
                name: "x".into(),
                start: -10,
                end: -5,
                value: Literal::Int(-1),
            }
        );
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
}
