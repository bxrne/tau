//! The line-oriented wire protocol, shared by the server and its clients.
//!
//! One executed statement maps to one [`Response`], which encodes to exactly one
//! line via [`Display`] and decodes from one line via [`Response::parse`]. Before
//! this module the encoder lived in the `tau` binary and every client re-derived
//! the decoding by sniffing string prefixes; keeping both halves here means the
//! grammar is defined once and the two ends cannot drift.
//!
//! Response shapes (value encoding uses the `Value` type tags `i`/`f`/`s`/`b`/`n`):
//!
//! ```text
//! OK                                         DDL / write success
//! VAL i42                                    point lookup hit (AT, REDUCE)
//! VAL NIL                                    point lookup miss
//! RANGE 2; 0:10:i1; 10:20:i2                 range scan
//! NAMES 2; foo; bar                          SHOW DATABASES / LENSES / USERS
//! GRANTS 1; alice main:RU                    SHOW GRANTS
//! LAYERS 2; 1:1717000000:0:3600; 2:...       HISTORY LENS
//! ERR <message>                              any failure
//! ```
//!
//! `LAYERS` entry: `<id>:<written_at_ms>:<min_start>:<max_end>`. The tau count is
//! not carried on the wire, so a parsed `LayerInfo` has `tau_count == 0`.

use std::fmt;

use crate::executor::{ExecError, LayerInfo, Output};
use crate::model::Timestamp;
use crate::storage::Codec;
use crate::users::Perm;
use crate::value::Value;

/// A single protocol response. Built from an executor [`Output`]/[`ExecError`] on
/// the server, reconstructed from a line by [`Response::parse`] on the client.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// `OK` - statement succeeded with no value (DDL / writes).
    Ok,
    /// `VAL ...` - point lookup; `None` is the `NIL` miss.
    Val(Option<Value>),
    /// `RANGE ...` - `(start, end, value)` segments.
    Range(Vec<(Timestamp, Timestamp, Value)>),
    /// `NAMES ...` - SHOW DATABASES / LENSES / USERS.
    Names(Vec<String>),
    /// `GRANTS ...` - per user, a list of `(database, perms)`.
    Grants(Vec<(String, Vec<(String, Perm)>)>),
    /// `LAYERS ...` - HISTORY LENS metadata.
    Layers(Vec<LayerInfo>),
    /// `STATUS ...` - SHOW STATUS key-value pairs.
    Status(Vec<(String, String)>),
    /// `ERR ...` - the message text (already rendered, no `ERR ` prefix).
    Err(String),
}

impl Response {
    /// Build the response for an executed statement, mapping the error case
    /// through [`encode_error`]. This is the server's single entry point: it no
    /// longer needs to decide success vs failure by inspecting a string.
    pub fn from_result(result: &Result<Output, ExecError>) -> Self {
        match result {
            Ok(o) => Response::from_output(o),
            Err(e) => Response::Err(encode_error(e)),
        }
    }

    /// Map a successful [`Output`] to its response shape.
    pub fn from_output(o: &Output) -> Self {
        match o {
            Output::Empty => Response::Ok,
            Output::Value(v) => Response::Val(v.clone()),
            Output::Range(segs) => Response::Range(segs.clone()),
            Output::Names(n) => Response::Names(n.clone()),
            Output::Grants(g) => Response::Grants(g.clone()),
            Output::LayerHistory(l) => Response::Layers(l.clone()),
            Output::Status(kv) => Response::Status(kv.clone()),
        }
    }

    /// `true` for the `ERR` variant - lets a client branch on outcome without
    /// re-parsing the message.
    pub fn is_err(&self) -> bool {
        matches!(self, Response::Err(_))
    }

    /// Map a wire response back to executor output (for DST / clients).
    pub fn into_output(self) -> Result<Output, ExecError> {
        match self {
            Response::Ok => Ok(Output::Empty),
            Response::Val(v) => Ok(Output::Value(v)),
            Response::Range(segs) => Ok(Output::Range(segs)),
            Response::Names(n) => Ok(Output::Names(n)),
            Response::Grants(g) => Ok(Output::Grants(g)),
            Response::Layers(l) => Ok(Output::LayerHistory(l)),
            Response::Status(kv) => Ok(Output::Status(kv)),
            Response::Err(msg) => Err(decode_exec_error(&msg)),
        }
    }

    /// Parse one response line (a trailing newline is tolerated).
    pub fn parse(line: &str) -> Result<Response, WireError> {
        let line = line.trim_end_matches(['\n', '\r']);
        if line == "OK" {
            return Ok(Response::Ok);
        }
        let (head, rest) = match line.split_once(' ') {
            Some((h, r)) => (h, r),
            None => return Err(WireError(format!("unrecognised response: {line:?}"))),
        };
        match head {
            "VAL" => {
                if rest == "NIL" {
                    Ok(Response::Val(None))
                } else {
                    Value::decode(rest)
                        .map(|v| Response::Val(Some(v)))
                        .ok_or_else(|| WireError(format!("bad value encoding: {rest:?}")))
                }
            }
            "ERR" => Ok(Response::Err(rest.to_string())),
            "RANGE" => {
                let mut segs = Vec::new();
                for item in items(rest) {
                    let mut p = item.splitn(3, ':');
                    let s = parse_int(p.next(), "range start")?;
                    let e = parse_int(p.next(), "range end")?;
                    let enc = p.next().ok_or_else(|| {
                        WireError(format!("range segment missing value: {item:?}"))
                    })?;
                    let v = Value::decode(enc)
                        .ok_or_else(|| WireError(format!("bad value encoding: {enc:?}")))?;
                    segs.push((s, e, v));
                }
                Ok(Response::Range(segs))
            }
            "NAMES" => Ok(Response::Names(items(rest).map(str::to_string).collect())),
            "GRANTS" => {
                let mut rows = Vec::new();
                for item in items(rest) {
                    let mut fields = item.split_whitespace();
                    let user = fields
                        .next()
                        .ok_or_else(|| WireError(format!("grants row missing user: {item:?}")))?
                        .to_string();
                    let mut grants = Vec::new();
                    for g in fields {
                        let (db, perm) = g
                            .split_once(':')
                            .ok_or_else(|| WireError(format!("bad grant token: {g:?}")))?;
                        let perm = Perm::parse(perm).map_err(WireError)?;
                        grants.push((db.to_string(), perm));
                    }
                    rows.push((user, grants));
                }
                Ok(Response::Grants(rows))
            }
            "LAYERS" => {
                let mut layers = Vec::new();
                for item in items(rest) {
                    let mut p = item.split(':');
                    let id = parse_u64(p.next(), "layer id")?;
                    let written_at = parse_int(p.next(), "layer written_at")?;
                    let min_start = parse_int(p.next(), "layer min_start")?;
                    let max_end = parse_int(p.next(), "layer max_end")?;
                    layers.push(LayerInfo {
                        id,
                        written_at,
                        min_start,
                        max_end,
                        tau_count: 0,
                    });
                }
                Ok(Response::Layers(layers))
            }
            "STATUS" => {
                let mut pairs = Vec::new();
                for item in items(rest) {
                    let (k, v) = item
                        .split_once(':')
                        .ok_or_else(|| WireError(format!("bad STATUS pair: {item:?}")))?;
                    pairs.push((k.to_string(), v.to_string()));
                }
                Ok(Response::Status(pairs))
            }
            other => Err(WireError(format!("unknown response head: {other:?}"))),
        }
    }
}

/// Split the body of a counted response (`<count>; a; b; c`) into its items,
/// dropping the leading count token.
fn items(rest: &str) -> impl Iterator<Item = &str> {
    rest.split("; ").skip(1)
}

fn parse_int(tok: Option<&str>, what: &str) -> Result<Timestamp, WireError> {
    tok.ok_or_else(|| WireError(format!("missing {what}")))?
        .parse()
        .map_err(|_| WireError(format!("bad {what}")))
}

fn parse_u64(tok: Option<&str>, what: &str) -> Result<u64, WireError> {
    tok.ok_or_else(|| WireError(format!("missing {what}")))?
        .parse()
        .map_err(|_| WireError(format!("bad {what}")))
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Response::Ok => f.write_str("OK"),
            Response::Val(None) => f.write_str("VAL NIL"),
            Response::Val(Some(v)) => write!(f, "VAL {}", v.encode()),
            Response::Range(segs) => {
                write!(f, "RANGE {}", segs.len())?;
                for (s, e, v) in segs {
                    write!(f, "; {}:{}:{}", s, e, v.encode())?;
                }
                Ok(())
            }
            Response::Names(names) => {
                write!(f, "NAMES {}", names.len())?;
                for n in names {
                    write!(f, "; {n}")?;
                }
                Ok(())
            }
            Response::Grants(rows) => {
                write!(f, "GRANTS {}", rows.len())?;
                for (user, grants) in rows {
                    write!(f, "; {user}")?;
                    for (db, perm) in grants {
                        write!(f, " {db}:{perm}")?;
                    }
                }
                Ok(())
            }
            Response::Layers(layers) => {
                write!(f, "LAYERS {}", layers.len())?;
                for l in layers {
                    write!(
                        f,
                        "; {}:{}:{}:{}",
                        l.id, l.written_at, l.min_start, l.max_end
                    )?;
                }
                Ok(())
            }
            Response::Status(kv) => {
                write!(f, "STATUS {}", kv.len())?;
                for (k, v) in kv {
                    write!(f, "; {k}:{v}")?;
                }
                Ok(())
            }
            Response::Err(msg) => write!(f, "ERR {msg}"),
        }
    }
}

/// Reconstruct an [`ExecError`] from its wire message (the text after `ERR `).
/// The inverse of [`encode_error`]; only the structured variants the client
/// branches on are recovered, everything else collapses to [`ExecError::InvalidExpr`].
fn decode_exec_error(msg: &str) -> ExecError {
    if let Some(name) = msg.strip_prefix("unknown lens: ") {
        return ExecError::UnknownLens(name.into());
    }
    if let Some(name) = msg.strip_prefix("unknown database: ") {
        return ExecError::UnknownDatabase(name.into());
    }
    if msg == "transaction already active" {
        return ExecError::TransactionAlreadyActive;
    }
    if msg == "no active transaction" {
        return ExecError::NoActiveTransaction;
    }
    ExecError::InvalidExpr(msg.into())
}

pub fn encode_error(e: &ExecError) -> String {
    match e {
        ExecError::NoActiveDatabase => "no active database".into(),
        ExecError::UnknownDatabase(n) => format!("unknown database: {n}"),
        ExecError::DuplicateDatabase(n) => format!("duplicate database: {n}"),
        ExecError::UnknownLens(n) => format!("unknown lens: {n}"),
        ExecError::DuplicateLens(n) => format!("duplicate lens: {n}"),
        ExecError::TypeMismatch {
            lens,
            expected,
            got,
        } => format!("type mismatch on {lens}: expected {expected:?}, got {got}"),
        ExecError::InvalidExpr(m) => format!("invalid expression: {m}"),
        ExecError::InvalidRange => "invalid range (start >= end)".into(),
        ExecError::Io(m) => format!("storage error: {m}"),
        ExecError::CycleDetected(n) => format!("cycle detected in derived lens: {n}"),
        ExecError::PermissionDenied(m) => format!("permission denied: {m}"),
        ExecError::DuplicateUser(n) => format!("duplicate user: {n}"),
        ExecError::UnknownUser(n) => format!("unknown user: {n}"),
        ExecError::TransactionAlreadyActive => "transaction already active".into(),
        ExecError::NoActiveTransaction => "no active transaction".into(),
    }
}

/// A response line that could not be parsed into a [`Response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed response: {}", self.0)
    }
}

impl std::error::Error for WireError {}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use hegel::generators::Generator;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[test]
    fn encodes_documented_shapes() {
        assert_eq!(Response::Ok.to_string(), "OK");
        assert_eq!(Response::Val(None).to_string(), "VAL NIL");
        assert_eq!(Response::Val(Some(Value::Int(42))).to_string(), "VAL i42");
        assert_eq!(
            Response::Range(vec![(0, 10, Value::Int(1)), (10, 20, Value::Int(2))]).to_string(),
            "RANGE 2; 0:10:i1; 10:20:i2"
        );
        assert_eq!(
            Response::Names(vec!["foo".into(), "bar".into()]).to_string(),
            "NAMES 2; foo; bar"
        );
        assert_eq!(
            Response::Grants(vec![(
                "alice".into(),
                vec![("main".into(), Perm::parse("RU").unwrap())]
            )])
            .to_string(),
            "GRANTS 1; alice main:RU"
        );
        assert_eq!(Response::Err("boom".into()).to_string(), "ERR boom");
    }

    #[test]
    fn layers_encode_four_fields() {
        let r = Response::Layers(vec![LayerInfo {
            id: 1,
            written_at: 1717000000,
            min_start: 0,
            max_end: 3600,
            tau_count: 60,
        }]);
        // tau_count is intentionally not on the wire.
        assert_eq!(r.to_string(), "LAYERS 1; 1:1717000000:0:3600");
    }

    fn roundtrip(r: &Response) {
        let line = r.to_string();
        let parsed = Response::parse(&line).expect("parse");
        assert_eq!(&parsed, r, "round-trip failed for line {line:?}");
    }

    #[test]
    fn roundtrips_common_shapes() {
        roundtrip(&Response::Ok);
        roundtrip(&Response::Val(None));
        roundtrip(&Response::Val(Some(Value::Int(-7))));
        roundtrip(&Response::Val(Some(Value::Bool(true))));
        roundtrip(&Response::Val(Some(Value::Null)));
        roundtrip(&Response::Val(Some(Value::Str(Arc::from("hi")))));
        roundtrip(&Response::Range(vec![]));
        roundtrip(&Response::Range(vec![
            (0, 10, Value::Int(1)),
            (10, 20, Value::Float(2.5)),
        ]));
        roundtrip(&Response::Names(vec![]));
        roundtrip(&Response::Names(vec!["a".into(), "b".into()]));
        roundtrip(&Response::Grants(vec![
            (
                "alice".into(),
                vec![("main".into(), Perm::parse("RU").unwrap())],
            ),
            ("bob".into(), vec![]),
        ]));
        roundtrip(&Response::Err("permission denied: nope".into()));
    }

    #[test]
    fn range_value_with_colon_survives() {
        // A string value containing ':' must not be split as extra fields.
        let r = Response::Range(vec![(0, 1, Value::Str(Arc::from("a:b")))]);
        let parsed = Response::parse(&r.to_string()).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn parse_tolerates_trailing_newline() {
        assert_eq!(Response::parse("OK\n").unwrap(), Response::Ok);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(Response::parse("WAT 1; 2").is_err());
        assert!(Response::parse("VAL zzz").is_err());
    }

    #[test]
    fn layers_parse_sets_zero_tau_count() {
        let line = "LAYERS 1; 9:123:0:3600";
        let parsed = Response::parse(line).unwrap();
        assert_eq!(
            parsed,
            Response::Layers(vec![LayerInfo {
                id: 9,
                written_at: 123,
                min_start: 0,
                max_end: 3600,
                tau_count: 0,
            }])
        );
    }

    #[hegel::test]
    fn parse_never_panics_on_arbitrary_text(tc: TestCase) {
        let s = tc.draw(gs::text().max_size(512));
        let _ = Response::parse(&s);
    }

    fn value_gen() -> impl Generator<Value> {
        gs::one_of(vec![
            gs::integers::<i64>().map(Value::Int).boxed(),
            gs::floats::<f64>()
                .filter(|f| f.is_finite())
                .map(Value::Float)
                .boxed(),
            gs::booleans().map(Value::Bool).boxed(),
            gs::just(Value::Null).boxed(),
            gs::text()
                .max_size(32)
                .filter(|s| !s.contains('\n') && !s.contains('\r'))
                .map(|s| Value::Str(std::sync::Arc::from(s.as_str())))
                .boxed(),
        ])
    }

    #[hegel::test]
    fn parse_roundtrips_val(tc: TestCase) {
        let v = tc.draw(value_gen());
        let r = Response::Val(Some(v));
        let line = r.to_string();
        let parsed = Response::parse(&line).expect("parse");
        assert_eq!(parsed, r);
    }

    #[hegel::test]
    fn parse_roundtrips_range(tc: TestCase) {
        let segs: Vec<(i64, i64, Value)> = tc
            .draw(
                gs::vecs(
                    gs::integers::<i64>()
                        .min_value(1)
                        .max_value(100)
                        .flat_map(|width| {
                            gs::integers::<i64>()
                                .min_value(0)
                                .max_value(20)
                                .flat_map(move |gap| {
                                    gs::integers::<i64>().map(move |v| (width, gap, Value::Int(v)))
                                })
                        }),
                )
                .max_size(8),
            )
            .into_iter()
            .scan(0i64, |cursor, (width, gap, v)| {
                let s = *cursor + gap;
                let e = s + width;
                *cursor = e;
                Some((s, e, v))
            })
            .collect();
        let r = Response::Range(segs);
        let line = r.to_string();
        let parsed = Response::parse(&line).expect("parse");
        assert_eq!(parsed, r);
    }

    #[hegel::test]
    fn parse_roundtrips_names(tc: TestCase) {
        let names: Vec<String> =
            tc.draw(gs::vecs(gs::from_regex(r"[a-z][a-z0-9_]{0,8}").fullmatch(true)).max_size(8));
        let r = Response::Names(names);
        let line = r.to_string();
        let parsed = Response::parse(&line).expect("parse");
        assert_eq!(parsed, r);
    }
}
