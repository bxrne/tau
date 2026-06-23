use std::sync::Arc;

use libtau::{AggFunc, Value};
use rand::Rng;
use rand::rngs::StdRng;

use crate::oracle::{DeriveSpec, Oracle, Ts};

pub const INT: &[&str] = &["a", "b"];
pub const FL: &str = "fl";
pub const BOOL: &str = "bl";
pub const SV: &str = "sv";
pub const DS: &str = "ds";
pub const XD: &str = "xd";

/// Named lenses used by the DST workload (aux + dynamic create/drop targets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lens {
    Aux,
    DynI,
    DynF,
    DynB,
    DynS,
}

impl Lens {
    pub const DYN: &[(Lens, &'static str)] = &[
        (Lens::DynI, "int"),
        (Lens::DynF, "float"),
        (Lens::DynB, "bool"),
        (Lens::DynS, "str"),
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Lens::Aux => "c",
            Lens::DynI => "dyn_i",
            Lens::DynF => "dyn_f",
            Lens::DynB => "dyn_b",
            Lens::DynS => "dyn_s",
        }
    }
}

const STR_VALUES: &[&str] = &["alpha", "beta", "gamma", "delta", "omega"];

#[derive(Clone, Debug)]
pub enum Payload {
    Int(Vec<(Ts, Ts, i64)>),
    Float(Vec<(Ts, Ts, f64)>),
    Bool(Vec<(Ts, Ts, bool)>),
    Str(Vec<(Ts, Ts, &'static str)>),
}

#[derive(Clone, Debug)]
pub enum Op {
    Append {
        lens: String,
        data: Payload,
    },
    At {
        lens: String,
        t: Ts,
    },
    Range {
        lens: String,
        start: Ts,
        end: Ts,
    },
    Reduce {
        lens: String,
        start: Ts,
        end: Ts,
        func: AggFunc,
    },
    CreateLens {
        name: String,
        ty: &'static str,
    },
    DropLens {
        name: String,
    },
    Derive {
        name: String,
        spec: DeriveSpec,
    },
    Xderive {
        name: String,
        spec: DeriveSpec,
        range: Option<(Ts, Ts)>,
    },
    #[allow(dead_code)]
    Ttl {
        lens: String,
        secs: Option<i64>,
    },
    UseDb(&'static str),
    StartTransaction,
    Commit,
    Rollback,
}

impl Op {
    /// The wire statement that drives this op against the SUT.
    pub fn to_sql(&self) -> String {
        match self {
            Op::Append { lens, data } => data.batch_sql(lens),
            Op::At { lens, t } => format!("AT LENS {lens} {t}"),
            Op::Range { lens, start, end } => format!("RANGE LENS {lens} {start} {end}"),
            Op::Reduce {
                lens,
                start,
                end,
                func,
            } => format!("REDUCE LENS {lens} {start} {end} USING {func}"),
            Op::CreateLens { name, ty } => format!("CREATE LENS {name} {ty}"),
            Op::DropLens { name } => format!("DROP LENS {name}"),
            Op::Derive { name, spec } => format!("DERIVE LENS {name} AS {} + {}", spec.a, spec.b),
            Op::Xderive { name, spec, range } => {
                let mut sql = format!("XDERIVE LENS {name} AS {} + {}", spec.a, spec.b);
                if let Some((s, e)) = range {
                    sql.push_str(&format!(" OVER {s} {e}"));
                }
                sql
            }
            Op::Ttl {
                lens,
                secs: Some(s),
            } => format!("SET TTL LENS {lens} {s}"),
            Op::Ttl { lens, secs: None } => format!("UNSET TTL LENS {lens}"),
            Op::UseDb(db) => format!("USE DATABASE {db}"),
            Op::StartTransaction => "START TRANSACTION".into(),
            Op::Commit => "COMMIT".into(),
            Op::Rollback => "ROLLBACK".into(),
        }
    }
}

impl Payload {
    pub fn to_values(&self) -> Vec<(Ts, Ts, Value)> {
        fn conv<T: Copy>(rows: &[(Ts, Ts, T)], f: impl Fn(T) -> Value) -> Vec<(Ts, Ts, Value)> {
            rows.iter().map(|&(s, e, v)| (s, e, f(v))).collect()
        }
        match self {
            Payload::Int(rows) => conv(rows, Value::Int),
            Payload::Float(rows) => conv(rows, Value::Float),
            Payload::Bool(rows) => conv(rows, Value::Bool),
            Payload::Str(rows) => conv(rows, |v| Value::Str(Arc::from(v))),
        }
    }

    pub fn batch_sql(&self, lens: &str) -> String {
        fn join<T>(rows: &[(Ts, Ts, T)], f: impl Fn(&T) -> String) -> String {
            rows.iter()
                .map(|(s, e, v)| format!("{s} {e} {}", f(v)))
                .collect::<Vec<_>>()
                .join(" ; ")
        }
        let body = match self {
            Payload::Int(rows) => join(rows, i64::to_string),
            Payload::Float(rows) => join(rows, f64::to_string),
            Payload::Bool(rows) => join(rows, bool::to_string),
            Payload::Str(rows) => join(rows, |v| format!("\"{v}\"")),
        };
        format!("BATCH APPEND LENS {lens} {{ {body} }}")
    }
}

/// Cursor-walked, sorted, non-overlapping taus with values drawn by `value`.
fn gen_taus<T>(
    rng: &mut StdRng,
    count: usize,
    mut value: impl FnMut(&mut StdRng) -> T,
) -> Vec<(Ts, Ts, T)> {
    let mut cur: Ts = rng.gen_range(0..2000);
    (0..count)
        .map(|_| {
            let s = cur + rng.gen_range(0..50);
            let e = s + rng.gen_range(1..100);
            let v = value(rng);
            cur = e;
            (s, e, v)
        })
        .collect()
}

pub fn gen_int_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, i64)> {
    gen_taus(rng, count, |r| r.gen_range(-1000i64..=1000))
}

pub fn gen_float_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, f64)> {
    gen_taus(rng, count, |r| r.gen_range(-100.0f64..100.0))
}

pub fn gen_bool_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, bool)> {
    gen_taus(rng, count, |r| r.gen_bool(0.5))
}

pub fn gen_str_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, &'static str)> {
    gen_taus(rng, count, |r| STR_VALUES[r.gen_range(0..STR_VALUES.len())])
}

pub fn extreme_ts(rng: &mut StdRng) -> Ts {
    match rng.gen_range(0..3u8) {
        0 => rng.gen_range(i64::MIN..i64::MIN + 10_000),
        1 => rng.gen_range(i64::MAX - 10_000..i64::MAX),
        _ => rng.gen_range(-5..5),
    }
}

pub fn rand_int_lens(rng: &mut StdRng) -> String {
    INT[rng.gen_range(0..INT.len())].to_string()
}

pub fn int_lens_for_db(o: &Oracle, rng: &mut StdRng) -> String {
    if o.active_db() == "default" {
        rand_int_lens(rng)
    } else {
        Lens::Aux.as_str().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_sql_roundtrip_format() {
        let sql = Payload::Int(vec![(0, 10, 42), (20, 30, 7)]).batch_sql("x");
        assert_eq!(sql, "BATCH APPEND LENS x { 0 10 42 ; 20 30 7 }");
    }

    #[test]
    fn payload_to_values_preserves_ints() {
        let v = Payload::Int(vec![(1, 2, 9)]).to_values();
        assert_eq!(v, vec![(1, 2, Value::Int(9))]);
    }
}
