//! Dynamic runtime value used by the query-language executor.
//!
//! A `Value` is the executor's analogue of [`crate::libtau::ql::ast::Literal`].
//! Every base lens in an executor-managed database stores `Value`s, so a
//! single `Database<Value>` can back any declared lens type (`int`, `float`,
//! `str`, `bool`, `bytes`). The executor enforces, per lens, that appended
//! values match the declared type — derived lenses are unconstrained because
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

use crate::libtau::ql::ast::{Literal, Type};
use crate::libtau::storage::Codec;

/// Dynamic value carried by every executor lens.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
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
            Literal::Str(s) => Value::Str(s),
            Literal::Bool(b) => Value::Bool(b),
            Literal::Null => Value::Null,
        }
    }
}

/// Percent-encode the three characters that would break the WAL line
/// format: space, colon, and the escape character itself.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            ':' => out.push_str("%3A"),
            c => out.push(c),
        }
    }
    out
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
        let a = chars.next()?;
        let b = chars.next()?;
        let code = u32::from_str_radix(&format!("{a}{b}"), 16).ok()?;
        out.push(char::from_u32(code)?);
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

    fn decode(s: &str) -> Option<Self> {
        let mut chars = s.chars();
        let tag = chars.next()?;
        let rest: String = chars.collect();
        match tag {
            'i' => rest.parse().ok().map(Value::Int),
            'f' => rest.parse().ok().map(Value::Float),
            's' => unescape(&rest).map(Value::Str),
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

    #[test]
    fn type_name_matches_variant() {
        assert_eq!(Value::Int(1).type_name(), "int");
        assert_eq!(Value::Float(1.0).type_name(), "float");
        assert_eq!(Value::Str("x".into()).type_name(), "str");
        assert_eq!(Value::Bool(true).type_name(), "bool");
        assert_eq!(Value::Null.type_name(), "null");
    }

    #[test]
    fn ty_returns_type_or_none_for_null() {
        assert_eq!(Value::Int(0).ty(), Some(Type::Int));
        assert_eq!(Value::Float(0.0).ty(), Some(Type::Float));
        assert_eq!(Value::Str("".into()).ty(), Some(Type::Str));
        assert_eq!(Value::Bool(false).ty(), Some(Type::Bool));
        assert_eq!(Value::Null.ty(), None);
    }

    #[test]
    fn from_literal_preserves_payload() {
        assert_eq!(Value::from(Literal::Int(42)), Value::Int(42));
        assert_eq!(Value::from(Literal::Float(1.5)), Value::Float(1.5));
        assert_eq!(
            Value::from(Literal::Str("hi".into())),
            Value::Str("hi".into())
        );
        assert_eq!(Value::from(Literal::Bool(true)), Value::Bool(true));
        assert_eq!(Value::from(Literal::Null), Value::Null);
    }

    #[test]
    fn codec_int_round_trip() {
        let v = Value::Int(-12345);
        assert_eq!(Value::decode(&v.encode()), Some(v));
    }

    #[test]
    fn codec_float_round_trip() {
        let v = Value::Float(1.23456);
        assert_eq!(Value::decode(&v.encode()), Some(v));
    }

    #[test]
    fn codec_bool_round_trip() {
        assert_eq!(Value::decode("b1"), Some(Value::Bool(true)));
        assert_eq!(Value::decode("b0"), Some(Value::Bool(false)));
    }

    #[test]
    fn codec_null_round_trip() {
        assert_eq!(Value::decode("n"), Some(Value::Null));
        assert_eq!(Value::Null.encode(), "n");
    }

    #[test]
    fn codec_str_escapes_space_colon_percent() {
        let v = Value::Str("a b:c%d".into());
        let enc = v.encode();
        assert!(!enc.contains(' '));
        assert!(!enc.contains(':'));
        // The escape character is allowed; only the *literal* % from the value
        // is escaped — the escapes themselves contain `%`.
        assert_eq!(Value::decode(&enc), Some(v));
    }

    #[test]
    fn codec_str_empty_round_trip() {
        let v = Value::Str(String::new());
        assert_eq!(Value::decode(&v.encode()), Some(v));
    }

    #[test]
    fn codec_rejects_unknown_tag() {
        assert_eq!(Value::decode("z42"), None);
    }

    #[test]
    fn codec_rejects_malformed_bool() {
        assert_eq!(Value::decode("b2"), None);
    }

    #[test]
    fn codec_rejects_null_with_payload() {
        assert_eq!(Value::decode("nx"), None);
    }
}
