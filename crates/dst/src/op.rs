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

impl Payload {
    pub fn len(&self) -> usize {
        match self {
            Payload::Int(v) => v.len(),
            Payload::Float(v) => v.len(),
            Payload::Bool(v) => v.len(),
            Payload::Str(v) => v.len(),
        }
    }

    pub fn to_values(&self) -> Vec<(Ts, Ts, Value)> {
        match self {
            Payload::Int(rows) => rows
                .iter()
                .map(|&(s, e, v)| (s, e, Value::Int(v)))
                .collect(),
            Payload::Float(rows) => rows
                .iter()
                .map(|&(s, e, v)| (s, e, Value::Float(v)))
                .collect(),
            Payload::Bool(rows) => rows
                .iter()
                .map(|&(s, e, v)| (s, e, Value::Bool(v)))
                .collect(),
            Payload::Str(rows) => rows
                .iter()
                .map(|&(s, e, v)| (s, e, Value::Str(Arc::from(v))))
                .collect(),
        }
    }

    pub fn batch_sql(&self, lens: &str) -> String {
        let body = match self {
            Payload::Int(rows) => rows
                .iter()
                .map(|(s, e, v)| format!("{s} {e} {v}"))
                .collect::<Vec<_>>()
                .join(" ; "),
            Payload::Float(rows) => rows
                .iter()
                .map(|(s, e, v)| format!("{s} {e} {v}"))
                .collect::<Vec<_>>()
                .join(" ; "),
            Payload::Bool(rows) => rows
                .iter()
                .map(|(s, e, v)| format!("{s} {e} {}", if *v { "true" } else { "false" }))
                .collect::<Vec<_>>()
                .join(" ; "),
            Payload::Str(rows) => rows
                .iter()
                .map(|(s, e, v)| format!("{s} {e} \"{v}\""))
                .collect::<Vec<_>>()
                .join(" ; "),
        };
        format!("BATCH APPEND LENS {lens} {{ {body} }}")
    }
}

pub fn gen_int_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, i64)> {
    let mut cur: Ts = rng.gen_range(0..2000);
    (0..count)
        .map(|_| {
            let s = cur + rng.gen_range(0..50);
            let e = s + rng.gen_range(1..100);
            let v = rng.gen_range(-1000i64..=1000);
            cur = e;
            (s, e, v)
        })
        .collect()
}

pub fn gen_float_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, f64)> {
    let mut cur: Ts = rng.gen_range(0..2000);
    (0..count)
        .map(|_| {
            let s = cur + rng.gen_range(0..50);
            let e = s + rng.gen_range(1..100);
            let v = rng.gen_range(-100.0f64..100.0);
            cur = e;
            (s, e, v)
        })
        .collect()
}

pub fn gen_bool_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, bool)> {
    let mut cur: Ts = rng.gen_range(0..2000);
    (0..count)
        .map(|_| {
            let s = cur + rng.gen_range(0..50);
            let e = s + rng.gen_range(1..100);
            let v = rng.gen_bool(0.5);
            cur = e;
            (s, e, v)
        })
        .collect()
}

pub fn gen_str_taus(rng: &mut StdRng, count: usize) -> Vec<(Ts, Ts, &'static str)> {
    let mut cur: Ts = rng.gen_range(0..2000);
    (0..count)
        .map(|_| {
            let s = cur + rng.gen_range(0..50);
            let e = s + rng.gen_range(1..100);
            let v = STR_VALUES[rng.gen_range(0..STR_VALUES.len())];
            cur = e;
            (s, e, v)
        })
        .collect()
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
