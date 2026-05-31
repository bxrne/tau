//! Dynamic runtime value used by the query-language executor.
//!
//! A `Value` is the executor's analogue of [`crate::ql::ast::Literal`].
//! Every base lens in an executor-managed database stores `Value`s, so a
//! single `Database<Value>` can back any declared lens type (`int`, `float`,
//! `str`, `bool`, `bytes`). The executor enforces, per lens, that appended
//! values match the declared type - derived lenses are unconstrained because
//! their type is determined by the expression they wrap.
//!
//! `Codec` is implemented so values can flow through the WAL.  The encoding
//! is a one-character tag followed by the payload:
//!
//! ```text
//! i<int>     i42
//! f<float>   f3.14
//! s<escaped> shello%20world      (space, ':' and '%' are percent-escaped)
//! b<0|1>     b1
//! n          n
//! ```
//!
//! The escape set keeps the WAL's whitespace/colon-separated wire format
//! unambiguous.

use std::sync::Arc;

use crate::ql::ast::{Literal, Type};
use crate::storage::Codec;

/// Dynamic value carried by every executor lens.
///
/// `Str` uses `Arc<str>` so that cloning a value (e.g. materialising range
/// segments) is an atomic reference-count bump rather than a heap allocation.
/// Creation cost is unchanged - one allocation bundles the Arc header and
/// string bytes together.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(Arc<str>),
    Bool(bool),
    Null,
}

impl Value {
    /// Returns the [`Type`] tag that matches this variant, or `None` for
    /// `Null` (which is type-compatible with any declared type).
    pub fn ty(&self) -> Option<Type> {
        match self {
            Value::Int(_) => Some(Type::Int),
            Value::Float(_) => Some(Type::Float),
            Value::Str(_) => Some(Type::Str),
            Value::Bool(_) => Some(Type::Bool),
            Value::Null => None,
        }
    }

    /// Human-readable tag, used in executor error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Bool(_) => "bool",
            Value::Null => "null",
        }
    }
}

impl From<Literal> for Value {
    fn from(l: Literal) -> Self {
        match l {
            Literal::Int(i) => Value::Int(i),
            Literal::Float(f) => Value::Float(f),
            // Both use Arc<str>; moving the Arc is a pointer copy, not a heap alloc.
            Literal::Str(s) => Value::Str(s),
            Literal::Bool(b) => Value::Bool(b),
            Literal::Null => Value::Null,
        }
    }
}

/// Reference conversion: allows converting a `&Literal` to a `Value` without
/// cloning the entire literal.  `Arc<str>` inside `Str` is bumped rather than
/// heap-allocated, so this is O(1) for all variants.
impl From<&Literal> for Value {
    fn from(l: &Literal) -> Self {
        match l {
            Literal::Int(i) => Value::Int(*i),
            Literal::Float(f) => Value::Float(*f),
            Literal::Str(s) => Value::Str(s.clone()), // Arc bump
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Null => Value::Null,
        }
    }
}

/// Percent-encode the three characters that would break the WAL line
/// format: space, colon, and the escape character itself.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(s, &mut out);
    out
}

/// Write the percent-encoded form of `s` directly into `buf` with no intermediate allocation.
fn escape_into(s: &str, buf: &mut String) {
    for ch in s.chars() {
        match ch {
            '%' => buf.push_str("%25"),
            ' ' => buf.push_str("%20"),
            ':' => buf.push_str("%3A"),
            c => buf.push(c),
        }
    }
}

/// Inverse of [`escape`]. Returns `None` on a malformed escape.
fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let a = chars.next()?.to_digit(16)?;
        let b = chars.next()?.to_digit(16)?;
        out.push(char::from_u32(a * 16 + b)?);
    }
    Some(out)
}

impl Codec for Value {
    fn encode(&self) -> String {
        match self {
            Value::Int(i) => format!("i{i}"),
            Value::Float(f) => format!("f{f}"),
            Value::Str(s) => format!("s{}", escape(s)),
            Value::Bool(b) => format!("b{}", if *b { 1 } else { 0 }),
            Value::Null => "n".into(),
        }
    }

    fn encode_into(&self, buf: &mut String) {
        use std::fmt::Write as _;
        match self {
            Value::Int(i) => {
                buf.push('i');
                let _ = write!(buf, "{i}");
            }
            Value::Float(f) => {
                buf.push('f');
                let _ = write!(buf, "{f}");
            }
            Value::Str(s) => {
                buf.push('s');
                escape_into(s, buf);
            }
            Value::Bool(b) => buf.push_str(if *b { "b1" } else { "b0" }),
            Value::Null => buf.push('n'),
        }
    }

    fn decode(s: &str) -> Option<Self> {
        let mut chars = s.chars();
        let tag = chars.next()?;
        let rest: String = chars.collect();
        match tag {
            'i' => rest.parse().ok().map(Value::Int),
            'f' => rest.parse().ok().map(Value::Float),
            's' => unescape(&rest).map(|s| Value::Str(Arc::from(s.as_str()))),
            'b' => match rest.as_str() {
                "1" => Some(Value::Bool(true)),
                "0" => Some(Value::Bool(false)),
                _ => None,
            },
            'n' if rest.is_empty() => Some(Value::Null),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;

    /// Generator over every `Value` variant.  Strings stay small and finite to
    /// keep counterexamples readable; floats avoid NaN because round-trip
    /// equality on NaN is intentionally false.
    fn value_gen() -> impl Generator<Value> {
        gs::one_of(vec![
            gs::integers::<i64>().map(Value::Int).boxed(),
            gs::floats::<f64>()
                .filter(|f| !f.is_nan())
                .map(Value::Float)
                .boxed(),
            gs::text()
                .max_size(64)
                .map(|s| Value::Str(Arc::from(s.as_str())))
                .boxed(),
            gs::booleans().map(Value::Bool).boxed(),
            gs::just(Value::Null).boxed(),
        ])
    }

    #[hegel::test]
    fn type_name_matches_variant(tc: TestCase) {
        let v = tc.draw(value_gen());
        let expected = match &v {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Bool(_) => "bool",
            Value::Null => "null",
        };
        assert_eq!(v.type_name(), expected);
    }

    #[hegel::test]
    fn ty_returns_type_for_non_null(tc: TestCase) {
        let v = tc.draw(value_gen());
        match (&v, v.ty()) {
            (Value::Int(_), Some(Type::Int)) => {}
            (Value::Float(_), Some(Type::Float)) => {}
            (Value::Str(_), Some(Type::Str)) => {}
            (Value::Bool(_), Some(Type::Bool)) => {}
            (Value::Null, None) => {}
            (val, ty) => panic!("mismatch: {:?} -> {:?}", val, ty),
        }
    }

    #[hegel::test]
    fn from_literal_owned_and_ref_agree(tc: TestCase) {
        let v = tc.draw(value_gen());
        let lit: Literal = match v.clone() {
            Value::Int(i) => Literal::Int(i),
            Value::Float(f) => Literal::Float(f),
            Value::Str(s) => Literal::Str(s),
            Value::Bool(b) => Literal::Bool(b),
            Value::Null => Literal::Null,
        };
        let owned: Value = Value::from(lit.clone());
        let by_ref: Value = Value::from(&lit);
        assert_eq!(owned, v);
        assert_eq!(by_ref, v);
    }

    #[hegel::test]
    fn codec_roundtrips_every_value(tc: TestCase) {
        let v = tc.draw(value_gen());
        // Float NaN is filtered out by the generator; for everything else
        // PartialEq must hold across encode + decode.
        let encoded = v.encode();
        let decoded = Value::decode(&encoded).expect("decode must succeed for produced encoding");
        assert_eq!(v, decoded);
    }

    #[hegel::test]
    fn codec_str_encoding_never_contains_separators(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(128));
        let v = Value::Str(Arc::from(s.as_str()));
        let enc = v.encode();
        // The WAL line format uses space and colon as separators - neither
        // may appear in an encoded value or replay would mis-tokenise.
        assert!(
            !enc[1..].contains(' '),
            "encoded str must not contain raw space: {enc:?}"
        );
        assert!(
            !enc[1..].contains(':'),
            "encoded str must not contain raw colon: {enc:?}"
        );
    }

    #[hegel::test]
    fn codec_decode_never_panics_on_arbitrary_input(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(128));
        let _ = Value::decode(&s);
    }

    #[hegel::test]
    fn codec_rejects_unknown_or_malformed_tags(tc: TestCase) {
        // Any first byte outside the known tag set must yield None.
        let known: [char; 5] = ['i', 'f', 's', 'b', 'n'];
        let tag = tc.draw(gs::characters().filter(move |c| !known.contains(c)));
        let payload = tc.draw(gs::text().max_size(16));
        let mut bytes = String::with_capacity(payload.len() + 4);
        bytes.push(tag);
        bytes.push_str(&payload);
        assert!(
            Value::decode(&bytes).is_none(),
            "tag {tag:?} should be rejected"
        );
    }
}
