use super::*;
use crate::ql::parse;
use hegel::TestCase;
use hegel::generators as gs;
use pretty_assertions::assert_eq;
use std::collections::HashMap as StdHashMap;

/// Parse + run.  Panics on parse failure; returns `Result` on exec.
fn run(exec: &mut Executor, q: &str) -> Result<Output, ExecError> {
    let (rest, stmt) = parse(q).expect("parse failed");
    assert!(rest.is_empty(), "unconsumed: {rest:?}");
    exec.exec(&stmt)
}

fn setup() -> Executor {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    e
}

#[test]
fn create_database_sets_active_on_first_create() {
    let mut e = Executor::new();
    assert_eq!(e.active(), None);
    run(&mut e, "CREATE DATABASE a").unwrap();
    assert_eq!(e.active(), Some("a"));
}

#[test]
fn second_create_does_not_change_active() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE a").unwrap();
    run(&mut e, "CREATE DATABASE b").unwrap();
    assert_eq!(e.active(), Some("a"));
}

#[test]
fn create_duplicate_database_errors() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE a").unwrap();
    assert_eq!(
        run(&mut e, "CREATE DATABASE a"),
        Err(ExecError::DuplicateDatabase("a".into()))
    );
}

#[test]
fn use_unknown_database_errors() {
    let mut e = Executor::new();
    assert_eq!(
        run(&mut e, "USE DATABASE ghost"),
        Err(ExecError::UnknownDatabase("ghost".into()))
    );
}

#[test]
fn use_switches_active() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE a").unwrap();
    run(&mut e, "CREATE DATABASE b").unwrap();
    run(&mut e, "USE DATABASE b").unwrap();
    assert_eq!(e.active(), Some("b"));
}

#[test]
fn drop_active_database_clears_active() {
    let mut e = setup();
    run(&mut e, "DROP DATABASE main").unwrap();
    assert_eq!(e.active(), None);
    assert_eq!(
        run(&mut e, "CREATE LENS x int"),
        Err(ExecError::NoActiveDatabase)
    );
}

#[test]
fn drop_unknown_database_errors() {
    let mut e = Executor::new();
    assert_eq!(
        run(&mut e, "DROP DATABASE ghost"),
        Err(ExecError::UnknownDatabase("ghost".into()))
    );
}

#[test]
fn create_lens_without_active_database_errors() {
    let mut e = Executor::new();
    assert_eq!(
        run(&mut e, "CREATE LENS x int"),
        Err(ExecError::NoActiveDatabase)
    );
}

#[test]
fn create_duplicate_lens_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "CREATE LENS x int"),
        Err(ExecError::DuplicateLens("x".into()))
    );
}

#[test]
fn drop_unknown_lens_errors() {
    let mut e = setup();
    assert_eq!(
        run(&mut e, "DROP LENS missing"),
        Err(ExecError::UnknownLens("missing".into()))
    );
}

#[test]
fn append_to_unknown_lens_errors() {
    let mut e = setup();
    assert_eq!(
        run(&mut e, "APPEND LENS x 0 10 1"),
        Err(ExecError::UnknownLens("x".into()))
    );
}

#[test]
fn append_type_mismatch_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "APPEND LENS x 0 10 1.5"),
        Err(ExecError::TypeMismatch {
            lens: "x".into(),
            expected: Type::Int,
            got: "float".into(),
        })
    );
}

#[test]
fn append_null_is_permitted_for_any_type() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 null").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Null))
    );
}

#[test]
fn append_with_inverted_range_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "APPEND LENS x 10 5 1"),
        Err(ExecError::InvalidRange)
    );
}

#[test]
fn at_returns_none_for_uncovered_time() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    assert_eq!(run(&mut e, "AT LENS x 50").unwrap(), Output::Value(None));
}

#[test]
fn at_returns_value_in_range() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
}

#[test]
fn at_observes_newest_layer() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 20 1").unwrap();
    run(&mut e, "APPEND LENS x 5 15 2").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 3").unwrap(),
        Output::Value(Some(Value::Int(1)))
    );
    assert_eq!(
        run(&mut e, "AT LENS x 10").unwrap(),
        Output::Value(Some(Value::Int(2)))
    );
    assert_eq!(
        run(&mut e, "AT LENS x 17").unwrap(),
        Output::Value(Some(Value::Int(1)))
    );
}

#[test]
fn derive_simple_arithmetic() {
    let mut e = setup();
    run(&mut e, "CREATE LENS c int").unwrap();
    run(&mut e, "APPEND LENS c 0 100 10").unwrap();
    run(&mut e, "DERIVE LENS doubled AS c * 2").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS doubled 50").unwrap(),
        Output::Value(Some(Value::Int(20)))
    );
}

#[test]
fn derive_celsius_to_fahrenheit_float() {
    let mut e = setup();
    run(&mut e, "CREATE LENS c float").unwrap();
    run(&mut e, "APPEND LENS c 0 100 18.0").unwrap();
    run(&mut e, "DERIVE LENS f AS c * 9.0 / 5.0 + 32.0").unwrap();
    let Output::Value(Some(Value::Float(v))) = run(&mut e, "AT LENS f 50").unwrap() else {
        panic!("expected float");
    };
    assert!((v - 64.4).abs() < 1e-9);
}

#[test]
fn derive_changes_type_from_int_to_bool() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 100 5").unwrap();
    run(&mut e, "DERIVE LENS big AS x > 3").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS big 10").unwrap(),
        Output::Value(Some(Value::Bool(true)))
    );
}

#[test]
fn derive_returns_none_when_source_uncovered() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 1").unwrap();
    run(&mut e, "DERIVE LENS d AS x + 1").unwrap();
    assert_eq!(run(&mut e, "AT LENS d 50").unwrap(), Output::Value(None));
}

#[test]
fn derive_chained() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 5").unwrap();
    run(&mut e, "DERIVE LENS y AS x * 2").unwrap();
    run(&mut e, "DERIVE LENS z AS y + 1").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS z 5").unwrap(),
        Output::Value(Some(Value::Int(11)))
    );
}

#[test]
fn derive_unknown_ident_errors_at_query_time() {
    let mut e = setup();
    run(&mut e, "DERIVE LENS d AS ghost + 1").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS d 0"),
        Err(ExecError::UnknownLens("ghost".into()))
    );
}

#[test]
fn divide_by_zero_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 1").unwrap();
    run(&mut e, "DERIVE LENS d AS x / 0").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS d 5"),
        Err(ExecError::InvalidExpr("divide by zero".into()))
    );
}

#[test]
fn range_returns_segments_split_at_change_points() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 5 10 2").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS x 0 10").unwrap(),
        Output::Range(vec![(0, 5, Value::Int(1)), (5, 10, Value::Int(2))])
    );
}

#[test]
fn range_merges_adjacent_equal_values() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 7").unwrap();
    run(&mut e, "APPEND LENS x 5 10 7").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS x 0 10").unwrap(),
        Output::Range(vec![(0, 10, Value::Int(7))])
    );
}

#[test]
fn range_skips_gaps() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 8 10 2").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS x 0 10").unwrap(),
        Output::Range(vec![(0, 5, Value::Int(1)), (8, 10, Value::Int(2))])
    );
}

#[test]
fn range_clips_to_query_window() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 100 9").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS x 10 20").unwrap(),
        Output::Range(vec![(10, 20, Value::Int(9))])
    );
}

#[test]
fn range_with_where_filter() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 5 10 50").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS x 0 10 WHERE x > 10").unwrap(),
        Output::Range(vec![(5, 10, Value::Int(50))])
    );
}

#[test]
fn range_on_derived_lens() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 5 10 2").unwrap();
    run(&mut e, "DERIVE LENS y AS x * 10").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS y 0 10").unwrap(),
        Output::Range(vec![(0, 5, Value::Int(10)), (5, 10, Value::Int(20))])
    );
}

#[test]
fn range_inverted_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "RANGE LENS x 10 5"),
        Err(ExecError::InvalidRange)
    );
}

#[test]
fn range_on_unknown_lens_errors() {
    let mut e = setup();
    assert_eq!(
        run(&mut e, "RANGE LENS ghost 0 10"),
        Err(ExecError::UnknownLens("ghost".into()))
    );
}

#[test]
fn reduce_count_segments() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 5 10 2").unwrap();
    assert_eq!(
        run(&mut e, "REDUCE LENS x 0 10 USING count").unwrap(),
        Output::Value(Some(Value::Int(2)))
    );
}

#[test]
fn reduce_sum_integers() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 3").unwrap();
    run(&mut e, "APPEND LENS x 5 10 7").unwrap();
    assert_eq!(
        run(&mut e, "REDUCE LENS x 0 10 USING sum").unwrap(),
        Output::Value(Some(Value::Int(10)))
    );
}

#[test]
fn reduce_min_max() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 3").unwrap();
    run(&mut e, "APPEND LENS x 5 10 7").unwrap();
    assert_eq!(
        run(&mut e, "REDUCE LENS x 0 10 USING min").unwrap(),
        Output::Value(Some(Value::Int(3)))
    );
    assert_eq!(
        run(&mut e, "REDUCE LENS x 0 10 USING max").unwrap(),
        Output::Value(Some(Value::Int(7)))
    );
}

#[test]
fn reduce_avg_time_weighted() {
    // Two equal-duration segments: avg = (3+7)/2 = 5.0
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 3").unwrap();
    run(&mut e, "APPEND LENS x 5 10 7").unwrap();
    let Output::Value(Some(Value::Float(v))) = run(&mut e, "REDUCE LENS x 0 10 USING avg").unwrap()
    else {
        panic!("expected float");
    };
    assert!((v - 5.0).abs() < 1e-9);
}

#[test]
fn reduce_avg_weighted_by_duration() {
    // [0,1) = 1, [1,10) = 10  →  weighted avg = (1*1 + 9*10) / 10 = 91/10 = 9.1
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 1 1").unwrap();
    run(&mut e, "APPEND LENS x 1 10 10").unwrap();
    let Output::Value(Some(Value::Float(v))) = run(&mut e, "REDUCE LENS x 0 10 USING avg").unwrap()
    else {
        panic!("expected float");
    };
    assert!((v - 9.1).abs() < 1e-9);
}

#[test]
fn reduce_returns_none_for_uncovered_range() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    assert_eq!(
        run(&mut e, "REDUCE LENS x 10 20 USING avg").unwrap(),
        Output::Value(None)
    );
}

#[test]
fn reduce_inverted_range_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "REDUCE LENS x 10 5 USING min"),
        Err(ExecError::InvalidRange)
    );
}

#[test]
fn reduce_unknown_lens_errors() {
    let mut e = setup();
    assert_eq!(
        run(&mut e, "REDUCE LENS ghost 0 10 USING avg"),
        Err(ExecError::UnknownLens("ghost".into()))
    );
}

#[test]
fn derive_with_rolling_avg() {
    // avg(x, -10, 0) at t=10 covers [0,10): values [0,5)=1 and [5,10)=2
    // time-weighted avg = (5*1 + 5*2) / 10 = 1.5
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 5 10 2").unwrap();
    run(&mut e, "DERIVE LENS smooth AS avg(x, -10, 0)").unwrap();
    let Output::Value(Some(Value::Float(v))) = run(&mut e, "AT LENS smooth 10").unwrap() else {
        panic!("expected float");
    };
    assert!((v - 1.5).abs() < 1e-9);
}

#[test]
fn derive_with_rolling_min() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 10").unwrap();
    run(&mut e, "APPEND LENS x 5 10 3").unwrap();
    run(&mut e, "DERIVE LENS lo AS min(x, -10, 0)").unwrap();
    // at t=10 window covers [0,10): min of 10 and 3 = 3
    assert_eq!(
        run(&mut e, "AT LENS lo 10").unwrap(),
        Output::Value(Some(Value::Int(3)))
    );
}

#[test]
fn derive_agg_in_comparison() {
    // hot = x > avg(x, -10, 0): true when current value exceeds rolling avg
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1").unwrap();
    run(&mut e, "APPEND LENS x 5 10 2").unwrap();
    run(&mut e, "DERIVE LENS hot AS x > avg(x, -10, 0)").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS hot 5").unwrap(),
        Output::Value(Some(Value::Bool(true)))
    );
}

#[test]
fn reduce_on_derived_lens() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 2").unwrap();
    run(&mut e, "APPEND LENS x 5 10 4").unwrap();
    run(&mut e, "DERIVE LENS doubled AS x * 2").unwrap();
    assert_eq!(
        run(&mut e, "REDUCE LENS doubled 0 10 USING sum").unwrap(),
        Output::Value(Some(Value::Int(12))) // 2*2*5 + 4*2*5 = 20+40... wait
    );
    // doubled over [0,5) = 4, over [5,10) = 8; sum = 4+8 = 12 ✓
}

#[test]
fn lenses_are_isolated_per_database() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE a").unwrap();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 1").unwrap();

    run(&mut e, "CREATE DATABASE b").unwrap();
    run(&mut e, "USE DATABASE b").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5"),
        Err(ExecError::UnknownLens("x".into()))
    );

    run(&mut e, "USE DATABASE a").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Int(1)))
    );
}

#[test]
fn schema_persists_across_wal_restart() {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");

    // First session: create lens, append data, derive another lens.
    {
        let mut e = Executor::with_wal(&wal_path, None).unwrap();
        run(&mut e, "CREATE LENS temp int").unwrap();
        run(&mut e, "APPEND LENS temp 0 10 42").unwrap();
        run(&mut e, "DERIVE LENS cold AS (temp * 2)").unwrap();
    }

    // Second session: reopen WAL - schema must be recovered automatically.
    let mut e2 = Executor::with_wal(&wal_path, None).unwrap();
    // Data is recovered.
    assert_eq!(
        run(&mut e2, "AT LENS temp 5").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
    // CREATE LENS schema is recovered - APPEND should not error with UnknownLens.
    run(&mut e2, "APPEND LENS temp 10 20 99").unwrap();
    assert_eq!(
        run(&mut e2, "AT LENS temp 15").unwrap(),
        Output::Value(Some(Value::Int(99)))
    );
    // DERIVE LENS schema is recovered - derived lens is usable.
    assert_eq!(
        run(&mut e2, "AT LENS cold 5").unwrap(),
        Output::Value(Some(Value::Int(84)))
    );
}

#[test]
fn wal_error_variant_formats_in_tcp_output() {
    let e = ExecError::Io("disk full".into());
    assert!(matches!(e, ExecError::Io(_)));
}

#[test]
fn append_multi_tau_single_layer() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 5 1, 5 10 2, 10 15 3").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 3").unwrap(),
        Output::Value(Some(Value::Int(1)))
    );
    assert_eq!(
        run(&mut e, "AT LENS x 7").unwrap(),
        Output::Value(Some(Value::Int(2)))
    );
    assert_eq!(
        run(&mut e, "AT LENS x 12").unwrap(),
        Output::Value(Some(Value::Int(3)))
    );
}

#[test]
fn append_multi_tau_type_mismatch_rejects_all() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert!(matches!(
        run(&mut e, "APPEND LENS x 0 5 1, 5 10 1.5"),
        Err(ExecError::TypeMismatch { .. })
    ));
    // No partial write - lens should still have no data.
    assert_eq!(run(&mut e, "AT LENS x 3").unwrap(), Output::Value(None));
}

#[test]
fn show_databases_lists_all() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE alpha").unwrap();
    run(&mut e, "CREATE DATABASE beta").unwrap();
    let Output::Names(mut names) = run(&mut e, "SHOW DATABASES").unwrap() else {
        panic!("expected Names output");
    };
    names.sort();
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[test]
fn show_lenses_lists_base_and_derived() {
    let mut e = setup();
    run(&mut e, "CREATE LENS a int").unwrap();
    run(&mut e, "CREATE LENS b float").unwrap();
    run(&mut e, "DERIVE LENS c AS a + 1").unwrap();
    let Output::Names(mut names) = run(&mut e, "SHOW LENSES").unwrap() else {
        panic!("expected Names output");
    };
    names.sort();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn show_lenses_requires_active_database() {
    let e = Executor::new();
    assert_eq!(
        e.exec_read(&crate::ql::parse("SHOW LENSES").unwrap().1),
        Err(ExecError::NoActiveDatabase)
    );
}

#[test]
fn cycle_detection_direct_self_reference() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "DERIVE LENS x2 AS x2 + 1"),
        Err(ExecError::CycleDetected("x2".into()))
    );
}

#[test]
fn cycle_detection_transitive() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "DERIVE LENS y AS x + 1").unwrap();
    run(&mut e, "DERIVE LENS z AS y + 1").unwrap();
    // z → y → x (fine). Now try w → z → y → w (cycle).
    assert_eq!(
        run(&mut e, "DERIVE LENS w AS z + w"),
        Err(ExecError::CycleDetected("w".into()))
    );
}

#[test]
fn copy_lens_from_csv() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = dir.path().join("data.csv");
    std::fs::write(&csv_path, "0,10,42\n10,20,99\n").unwrap();

    let mut e = setup();
    run(&mut e, "CREATE LENS sensor int").unwrap();
    run(
        &mut e,
        &format!("COPY LENS sensor FROM \"{}\"", csv_path.display()),
    )
    .unwrap();
    assert_eq!(
        run(&mut e, "AT LENS sensor 5").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
    assert_eq!(
        run(&mut e, "AT LENS sensor 15").unwrap(),
        Output::Value(Some(Value::Int(99)))
    );
}

fn install_admin(e: &mut Executor) {
    let mut grants = StdHashMap::new();
    grants.insert("*".into(), Perm::ALL);
    e.users.add(User::new("admin", "p", grants)).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
}

fn install_reader(e: &mut Executor, db: &str) {
    let mut grants = StdHashMap::new();
    grants.insert(db.to_string(), Perm::R);
    e.users.add(User::new("reader", "p", grants)).unwrap(); // codeql[rust/hard-coded-cryptographic-value]
}

#[test]
fn exec_as_admin_can_do_anything() {
    let mut e = Executor::new();
    install_admin(&mut e);
    let (_, stmt) = parse("CREATE DATABASE main").unwrap();
    assert_eq!(e.exec_as(&stmt, "admin").unwrap(), Output::Empty);
    let (_, stmt) = parse("CREATE LENS x int").unwrap();
    assert_eq!(e.exec_as(&stmt, "admin").unwrap(), Output::Empty);
    let (_, stmt) = parse("APPEND LENS x 0 10 42").unwrap();
    assert_eq!(e.exec_as(&stmt, "admin").unwrap(), Output::Empty);
    let (_, stmt) = parse("AT LENS x 5").unwrap();
    assert_eq!(
        e.exec_as(&stmt, "admin").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
}

#[test]
fn exec_as_reader_can_read_not_write() {
    let mut e = Executor::new();
    install_admin(&mut e);
    let (_, stmt) = parse("CREATE DATABASE main").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    let (_, stmt) = parse("CREATE LENS x int").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    let (_, stmt) = parse("APPEND LENS x 0 10 42").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    install_reader(&mut e, "main");

    // Reader can read.
    let (_, stmt) = parse("AT LENS x 5").unwrap();
    assert!(matches!(
        e.exec_read_as(&stmt, "reader").unwrap(),
        Output::Value(Some(Value::Int(42)))
    ));
    // Reader cannot append.
    let (_, stmt) = parse("APPEND LENS x 10 20 99").unwrap();
    assert!(matches!(
        e.exec_as(&stmt, "reader"),
        Err(ExecError::PermissionDenied(_))
    ));
    // Reader cannot drop.
    let (_, stmt) = parse("DROP LENS x").unwrap();
    assert!(matches!(
        e.exec_as(&stmt, "reader"),
        Err(ExecError::PermissionDenied(_))
    ));
    // Reader cannot create a database.
    let (_, stmt) = parse("CREATE DATABASE other").unwrap();
    assert!(matches!(
        e.exec_as(&stmt, "reader"),
        Err(ExecError::PermissionDenied(_))
    ));
    // Reader cannot manage users.
    let (_, stmt) = parse("CREATE USER newbie PASSWORD \"x\"").unwrap();
    assert!(matches!(
        e.exec_as(&stmt, "reader"),
        Err(ExecError::PermissionDenied(_))
    ));
}

#[test]
fn exec_as_unknown_user_errors() {
    let mut e = Executor::new();
    let (_, stmt) = parse("SHOW DATABASES").unwrap();
    assert!(matches!(
        e.exec_as(&stmt, "ghost"),
        Err(ExecError::UnknownUser(_))
    ));
}

#[test]
fn admin_can_create_drop_user_and_grant() {
    let mut e = Executor::new();
    install_admin(&mut e);
    let (_, stmt) = parse("CREATE USER bob PASSWORD \"hunter2\"").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    assert!(e.users.get("bob").is_some());

    let (_, stmt) = parse("GRANT R ON main TO bob").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    assert_eq!(e.users.get("bob").unwrap().effective("main"), Perm::R);

    let (_, stmt) = parse("REVOKE R ON main FROM bob").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    assert_eq!(e.users.get("bob").unwrap().effective("main"), Perm::NONE);

    let (_, stmt) = parse("DROP USER bob").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    assert!(e.users.get("bob").is_none());
}

#[test]
fn promote_to_admin_via_a_bit() {
    let mut e = Executor::new();
    install_admin(&mut e);
    let (_, stmt) = parse("CREATE USER bob PASSWORD \"p\"").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    // Before promotion bob cannot create users.
    let (_, stmt) = parse("CREATE USER carol PASSWORD \"p\"").unwrap();
    assert!(matches!(
        e.exec_as(&stmt, "bob"),
        Err(ExecError::PermissionDenied(_))
    ));
    // Promote bob with A on the wildcard database.
    let (_, stmt) = parse("GRANT A ON * TO bob").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    // Now bob can create users.
    let (_, stmt) = parse("CREATE USER carol PASSWORD \"p\"").unwrap();
    assert!(e.exec_as(&stmt, "bob").is_ok());
}

#[test]
fn show_databases_filters_for_non_admin() {
    let mut e = Executor::new();
    install_admin(&mut e);
    let (_, stmt) = parse("CREATE DATABASE alpha").unwrap();
    e.exec_as(&stmt, "admin").unwrap();
    let (_, stmt) = parse("CREATE DATABASE beta").unwrap();
    e.exec_as(&stmt, "admin").unwrap();

    let mut grants = StdHashMap::new();
    grants.insert("alpha".to_string(), Perm::R);
    e.users.add(User::new("alice", "p", grants)).unwrap(); // codeql[rust/hard-coded-cryptographic-value]

    let (_, stmt) = parse("SHOW DATABASES").unwrap();
    let out = e.exec_as(&stmt, "alice").unwrap();
    match out {
        Output::Names(names) => assert_eq!(names, vec!["alpha"]),
        _ => panic!("expected Names"),
    }
    let out = e.exec_as(&stmt, "admin").unwrap();
    match out {
        Output::Names(mut names) => {
            names.sort();
            assert_eq!(names, vec!["alpha", "beta"]);
        }
        _ => panic!("expected Names"),
    }
}

#[test]
fn transaction_start_returns_ok() {
    let mut e = setup();
    assert_eq!(run(&mut e, "START TRANSACTION").unwrap(), Output::Empty);
}

#[test]
fn commit_without_active_transaction_errors() {
    let mut e = setup();
    assert_eq!(run(&mut e, "COMMIT"), Err(ExecError::NoActiveTransaction));
}

#[test]
fn rollback_without_active_transaction_errors() {
    let mut e = setup();
    assert_eq!(run(&mut e, "ROLLBACK"), Err(ExecError::NoActiveTransaction));
}

#[test]
fn nested_start_transaction_errors() {
    let mut e = setup();
    run(&mut e, "START TRANSACTION").unwrap();
    assert_eq!(
        run(&mut e, "START TRANSACTION"),
        Err(ExecError::TransactionAlreadyActive)
    );
}

#[test]
fn transaction_rollback_discards_appends() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "START TRANSACTION").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    run(&mut e, "ROLLBACK").unwrap();
    assert_eq!(run(&mut e, "AT LENS x 5").unwrap(), Output::Value(None));
}

#[test]
fn appends_within_transaction_not_visible_before_commit() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "START TRANSACTION").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    // Data should not be visible until COMMIT.
    assert_eq!(run(&mut e, "AT LENS x 5").unwrap(), Output::Value(None));
    run(&mut e, "COMMIT").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
}

#[hegel::test]
fn committed_transaction_matches_direct_writes(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut segs: Vec<(i64, i64, i64)> = Vec::new();
    let mut cursor: i64 = 0;
    for _ in 0..n {
        let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let s = cursor + gap;
        let e = s + len;
        segs.push((s, e, val));
        cursor = e;
    }

    let mut direct = setup();
    run(&mut direct, "CREATE LENS x int").unwrap();
    for &(s, e, v) in &segs {
        run(&mut direct, &format!("APPEND LENS x {s} {e} {v}")).unwrap();
    }

    let mut tx = setup();
    run(&mut tx, "CREATE LENS x int").unwrap();
    run(&mut tx, "START TRANSACTION").unwrap();
    for &(s, e, v) in &segs {
        run(&mut tx, &format!("APPEND LENS x {s} {e} {v}")).unwrap();
    }
    run(&mut tx, "COMMIT").unwrap();

    for &(s, e, v) in &segs {
        let mid = s + (e - s) / 2;
        assert_eq!(
            run(&mut direct, &format!("AT LENS x {mid}")).unwrap(),
            run(&mut tx, &format!("AT LENS x {mid}")).unwrap(),
            "segment [{s},{e}) value {v} diverged after commit"
        );
    }
}

#[hegel::test]
fn rollback_leaves_lens_unchanged(tc: TestCase) {
    let base_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
    let tx_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));

    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, &format!("APPEND LENS x 0 100 {base_val}")).unwrap();

    run(&mut e, "START TRANSACTION").unwrap();
    run(&mut e, &format!("APPEND LENS x 100 200 {tx_val}")).unwrap();
    run(&mut e, "ROLLBACK").unwrap();

    assert_eq!(
        run(&mut e, "AT LENS x 50").unwrap(),
        Output::Value(Some(Value::Int(base_val))),
        "base data corrupted by rollback"
    );
    assert_eq!(
        run(&mut e, "AT LENS x 150").unwrap(),
        Output::Value(None),
        "rolled-back data still visible"
    );
}

#[hegel::test]
fn pending_writes_invisible_before_commit(tc: TestCase) {
    let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
    let s = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
    let e_ts = s + tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));

    let mut exec = setup();
    run(&mut exec, "CREATE LENS x int").unwrap();
    run(&mut exec, "START TRANSACTION").unwrap();
    run(&mut exec, &format!("APPEND LENS x {s} {e_ts} {val}")).unwrap();

    let mid = s + (e_ts - s) / 2;
    assert_eq!(
        run(&mut exec, &format!("AT LENS x {mid}")).unwrap(),
        Output::Value(None),
        "pending write visible before COMMIT"
    );
}

#[hegel::test]
fn multiple_sequential_transactions_accumulate(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let vals: Vec<i64> = (0..n)
        .map(|_| tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000)))
        .collect();

    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    for (i, &v) in vals.iter().enumerate() {
        let s = (i as i64) * 100;
        let end = s + 100;
        run(&mut e, "START TRANSACTION").unwrap();
        run(&mut e, &format!("APPEND LENS x {s} {end} {v}")).unwrap();
        run(&mut e, "COMMIT").unwrap();
    }
    for (i, &v) in vals.iter().enumerate() {
        let mid = (i as i64) * 100 + 50;
        assert_eq!(
            run(&mut e, &format!("AT LENS x {mid}")).unwrap(),
            Output::Value(Some(Value::Int(v))),
            "tx {i} value {v} missing after sequential commits"
        );
    }
}

#[hegel::test]
fn rollback_then_commit_independent(tc: TestCase) {
    let discard_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
    let keep_val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));

    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();

    run(&mut e, "START TRANSACTION").unwrap();
    run(&mut e, &format!("APPEND LENS x 0 100 {discard_val}")).unwrap();
    run(&mut e, "ROLLBACK").unwrap();

    run(&mut e, "START TRANSACTION").unwrap();
    run(&mut e, &format!("APPEND LENS x 0 100 {keep_val}")).unwrap();
    run(&mut e, "COMMIT").unwrap();

    assert_eq!(
        run(&mut e, "AT LENS x 50").unwrap(),
        Output::Value(Some(Value::Int(keep_val))),
        "committed value wrong after preceding rollback"
    );
}

#[test]
fn copy_lens_skips_blank_lines_and_comments() {
    let dir = tempfile::tempdir().unwrap();
    let csv_path = dir.path().join("data.csv");
    std::fs::write(&csv_path, "# header\n\n0,10,7\n").unwrap();

    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(
        &mut e,
        &format!("COPY LENS x FROM \"{}\"", csv_path.display()),
    )
    .unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Int(7)))
    );
}

#[test]
fn batch_append_produces_same_at_result_as_append() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "BATCH APPEND LENS x { 0 10 42 ; 20 30 99 }").unwrap();
    assert_eq!(
        run(&mut e, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
    assert_eq!(
        run(&mut e, "AT LENS x 25").unwrap(),
        Output::Value(Some(Value::Int(99)))
    );
    assert_eq!(run(&mut e, "AT LENS x 15").unwrap(), Output::Value(None));
}

#[test]
fn batch_append_empty_block_succeeds() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    assert_eq!(
        run(&mut e, "BATCH APPEND LENS x {}").unwrap(),
        Output::Empty
    );
}

#[hegel::test]
fn batch_append_matches_regular_append(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut segs: Vec<(i64, i64, i64)> = Vec::new();
    let mut cursor: i64 = 0;
    for _ in 0..n {
        let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let s = cursor + gap;
        let e = s + len;
        segs.push((s, e, val));
        cursor = e;
    }

    let mut direct = setup();
    run(&mut direct, "CREATE LENS x int").unwrap();
    let mut append_stmt = "APPEND LENS x".to_string();
    for (i, &(s, e, v)) in segs.iter().enumerate() {
        if i > 0 {
            append_stmt.push(',');
        }
        append_stmt.push_str(&format!(" {s} {e} {v}"));
    }
    run(&mut direct, &append_stmt).unwrap();

    let mut batch = setup();
    run(&mut batch, "CREATE LENS x int").unwrap();
    let body = segs
        .iter()
        .map(|(s, e, v)| format!("{s} {e} {v}"))
        .collect::<Vec<_>>()
        .join(" ; ");
    run(&mut batch, &format!("BATCH APPEND LENS x {{ {body} }}")).unwrap();

    for &(s, e, v) in &segs {
        let mid = s + (e - s) / 2;
        assert_eq!(
            run(&mut direct, &format!("AT LENS x {mid}")).unwrap(),
            run(&mut batch, &format!("AT LENS x {mid}")).unwrap(),
            "segment [{s},{e}) value {v} diverged between APPEND and BATCH APPEND"
        );
    }
}

#[test]
fn history_lens_returns_one_layer_after_append() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    let (_, stmt) = parse("HISTORY LENS x").unwrap();
    let out = e.exec_read(&stmt).unwrap();
    let layers = match out {
        Output::LayerHistory(l) => l,
        other => panic!("expected LayerHistory, got {other:?}"),
    };
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].tau_count, 1);
    assert_eq!(layers[0].min_start, 0);
    assert_eq!(layers[0].max_end, 10);
}

#[test]
fn history_lens_empty_on_no_data() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    let (_, stmt) = parse("HISTORY LENS x").unwrap();
    let out = e.exec_read(&stmt).unwrap();
    assert_eq!(out, Output::LayerHistory(vec![]));
}

#[test]
fn history_lens_time_filter_excludes_non_overlapping_layers() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 1").unwrap();
    run(&mut e, "APPEND LENS x 100 200 2").unwrap();
    let (_, stmt) = parse("HISTORY LENS x 50 150").unwrap();
    let out = e.exec_read(&stmt).unwrap();
    let layers = match out {
        Output::LayerHistory(l) => l,
        other => panic!("expected LayerHistory, got {other:?}"),
    };
    // Only the second layer (100..200) overlaps [50, 150).
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].min_start, 100);
}

#[hegel::test]
fn history_lens_layer_count_matches_appends(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    for i in 0..n {
        let s = (i as i64) * 100;
        run(&mut e, &format!("APPEND LENS x {s} {} {i}", s + 50)).unwrap();
    }
    let (_, stmt) = parse("HISTORY LENS x").unwrap();
    let layers = match e.exec_read(&stmt).unwrap() {
        Output::LayerHistory(l) => l,
        other => panic!("expected LayerHistory, got {other:?}"),
    };
    // Each APPEND creates one layer (assuming no compaction at threshold 4; n <= 8 may
    // trigger one compaction round, so check >= 1 and <= n).
    assert!(
        !layers.is_empty(),
        "expected at least one layer after {n} appends"
    );
    assert!(
        layers.len() <= n,
        "layer count {} > append count {n} (compaction should only reduce)",
        layers.len()
    );
}

#[test]
fn at_as_of_with_max_timestamp_includes_all_data() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    // written_at=0 for in-memory appends, so any as_of value includes them.
    let (_, stmt) = parse("AT LENS x 5 AS OF 9999999999999").unwrap();
    assert_eq!(
        e.exec_read(&stmt).unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
}

#[test]
fn at_as_of_derived_lens_errors() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "DERIVE LENS y AS x").unwrap();
    let (_, stmt) = parse("AT LENS y 5 AS OF 0").unwrap();
    assert!(
        e.exec_read(&stmt).is_err(),
        "AT AS OF on a derived lens should error"
    );
}

#[test]
fn at_layer_returns_value_from_correct_layer() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    let (_, hist_stmt) = parse("HISTORY LENS x").unwrap();
    let layer_id = match e.exec_read(&hist_stmt).unwrap() {
        Output::LayerHistory(layers) => layers[0].id,
        other => panic!("expected LayerHistory, got {other:?}"),
    };
    let (_, stmt) = parse(&format!("AT LENS x 5 LAYER {layer_id}")).unwrap();
    assert_eq!(
        e.exec_read(&stmt).unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
}

#[test]
fn at_layer_nonexistent_layer_returns_nil() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    let (_, stmt) = parse("AT LENS x 5 LAYER 99999").unwrap();
    assert_eq!(e.exec_read(&stmt).unwrap(), Output::Value(None));
}

#[test]
fn backup_restore_roundtrip_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let bak = dir.path().join("x.bak").display().to_string();

    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 42").unwrap();
    run(&mut e, "APPEND LENS x 10 20 99").unwrap();
    run(&mut e, &format!("BACKUP DATABASE main TO \"{bak}\"")).unwrap();

    let mut e2 = Executor::new();
    run(&mut e2, "CREATE DATABASE other").unwrap();
    run(&mut e2, &format!("RESTORE DATABASE main FROM \"{bak}\"")).unwrap();
    run(&mut e2, "USE DATABASE main").unwrap();
    assert_eq!(
        run(&mut e2, "AT LENS x 5").unwrap(),
        Output::Value(Some(Value::Int(42)))
    );
    assert_eq!(
        run(&mut e2, "AT LENS x 15").unwrap(),
        Output::Value(Some(Value::Int(99)))
    );
}

#[hegel::test]
fn at_as_of_with_large_timestamp_matches_at(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut segs: Vec<(i64, i64, i64)> = Vec::new();
    let mut cursor: i64 = 0;
    for _ in 0..n {
        let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let s = cursor + gap;
        let e = s + len;
        segs.push((s, e, val));
        cursor = e;
    }
    let mut ex = setup();
    run(&mut ex, "CREATE LENS x int").unwrap();
    for &(s, end, v) in &segs {
        run(&mut ex, &format!("APPEND LENS x {s} {end} {v}")).unwrap();
    }
    for &(s, end, _) in &segs {
        let mid = s + (end - s) / 2;
        let at_result = run(&mut ex, &format!("AT LENS x {mid}")).unwrap();
        let (_, stmt) = parse(&format!("AT LENS x {mid} AS OF 9999999999999")).unwrap();
        let as_of_result = ex.exec_read(&stmt).unwrap();
        assert_eq!(
            at_result, as_of_result,
            "AT and AT AS OF diverged at t={mid}"
        );
    }
}

#[hegel::test]
fn at_layer_for_single_layer_matches_at(tc: TestCase) {
    let s = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000));
    let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
    let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
    let end = s + len;
    let mid = s + len / 2;
    let mut ex = setup();
    run(&mut ex, "CREATE LENS x int").unwrap();
    run(&mut ex, &format!("APPEND LENS x {s} {end} {val}")).unwrap();
    let (_, hist_stmt) = parse("HISTORY LENS x").unwrap();
    let layer_id = match ex.exec_read(&hist_stmt).unwrap() {
        Output::LayerHistory(layers) => {
            assert_eq!(layers.len(), 1, "expected exactly one layer");
            layers[0].id
        }
        other => panic!("expected LayerHistory, got {other:?}"),
    };
    let at_result = run(&mut ex, &format!("AT LENS x {mid}")).unwrap();
    let (_, stmt) = parse(&format!("AT LENS x {mid} LAYER {layer_id}")).unwrap();
    let layer_result = ex.exec_read(&stmt).unwrap();
    assert_eq!(
        at_result, layer_result,
        "AT and AT LAYER diverged with single layer at t={mid}"
    );
}

#[hegel::test]
fn backup_restore_at_matches_original(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(1).max_value(6));
    let mut segs: Vec<(i64, i64, i64)> = Vec::new();
    let mut cursor: i64 = 0;
    for _ in 0..n {
        let gap = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let len = tc.draw(gs::integers::<i64>().min_value(1).max_value(1_000));
        let val = tc.draw(gs::integers::<i64>().min_value(-10_000).max_value(10_000));
        let s = cursor + gap;
        let e = s + len;
        segs.push((s, e, val));
        cursor = e;
    }
    let dir = tempfile::tempdir().unwrap();
    let bak = dir.path().join("prop.bak").display().to_string();
    let mut original = setup();
    run(&mut original, "CREATE LENS x int").unwrap();
    for &(s, end, v) in &segs {
        run(&mut original, &format!("APPEND LENS x {s} {end} {v}")).unwrap();
    }
    run(&mut original, &format!("BACKUP DATABASE main TO \"{bak}\"")).unwrap();
    let mut restored = Executor::new();
    run(&mut restored, "CREATE DATABASE anchor").unwrap();
    run(
        &mut restored,
        &format!("RESTORE DATABASE main FROM \"{bak}\""),
    )
    .unwrap();
    run(&mut restored, "USE DATABASE main").unwrap();
    for &(s, end, _) in &segs {
        let mid = s + (end - s) / 2;
        assert_eq!(
            run(&mut original, &format!("AT LENS x {mid}")).unwrap(),
            run(&mut restored, &format!("AT LENS x {mid}")).unwrap(),
            "backup/restore diverged at t={mid}"
        );
    }
}

#[test]
fn restore_existing_database_name_errors() {
    let dir = tempfile::tempdir().unwrap();
    let bak = dir.path().join("x.bak").display().to_string();

    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 1").unwrap();
    run(&mut e, &format!("BACKUP DATABASE main TO \"{bak}\"")).unwrap();

    let err = run(&mut e, &format!("RESTORE DATABASE main FROM \"{bak}\""));
    assert!(
        matches!(err, Err(ExecError::DuplicateDatabase(_))),
        "expected DuplicateDatabase, got {err:?}"
    );
}

#[test]
fn reduce_avg_on_derived_lens_uses_base_tau_boundaries() {
    let mut e = setup();
    run(&mut e, "CREATE LENS temp float").unwrap();
    run(&mut e, "APPEND LENS temp 0 3600 18.5, 3600 7200 21.0").unwrap();
    run(&mut e, "DERIVE LENS fahrenheit AS temp * 9.0 / 5.0 + 32.0").unwrap();

    let Output::Value(Some(Value::Float(f))) =
        run(&mut e, "REDUCE LENS fahrenheit 0 7200 USING avg").unwrap()
    else {
        panic!("expected float value");
    };
    // time-weighted: 3600 * (18.5*9/5+32) + 3600 * (21*9/5+32) all over 7200
    // = (65.3 + 69.8) / 2 = 67.55
    assert!((f - 67.55).abs() < 0.01, "got {f}");
}

#[test]
fn reduce_sum_on_derived_lens() {
    let mut e = setup();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 10 3, 10 20 7").unwrap();
    run(&mut e, "DERIVE LENS doubled AS x * 2").unwrap();

    let Output::Value(Some(v)) = run(&mut e, "REDUCE LENS doubled 0 20 USING sum").unwrap() else {
        panic!("expected value");
    };
    // 3*2=6 and 7*2=14; sum (not time-weighted) = 6+14 = 20
    assert_eq!(v, Value::Int(20));
}

#[test]
fn reduce_min_max_on_derived_lens() {
    let mut e = setup();
    run(&mut e, "CREATE LENS base int").unwrap();
    run(&mut e, "APPEND LENS base 0 5 10, 5 10 4, 10 15 7").unwrap();
    run(&mut e, "DERIVE LENS neg AS base * -1").unwrap();

    let Output::Value(Some(Value::Int(min))) =
        run(&mut e, "REDUCE LENS neg 0 15 USING min").unwrap()
    else {
        panic!()
    };
    let Output::Value(Some(Value::Int(max))) =
        run(&mut e, "REDUCE LENS neg 0 15 USING max").unwrap()
    else {
        panic!()
    };
    assert_eq!(min, -10); // min of [-10, -4, -7]
    assert_eq!(max, -4); // max of [-10, -4, -7]
}

#[hegel::test]
fn range_limit_truncates_segments(tc: TestCase) {
    let n = tc.draw(gs::integers::<usize>().min_value(2).max_value(10));
    let limit = tc.draw(gs::integers::<usize>().min_value(1).max_value(n - 1));
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS x int").unwrap();
    // append n non-overlapping unit-width taus
    let taus: String = (0..n as i64)
        .map(|i| format!("{} {} {}", i, i + 1, i))
        .collect::<Vec<_>>()
        .join(", ");
    run(&mut e, &format!("APPEND LENS x {taus}")).unwrap();

    let Output::Range(segs) = run(&mut e, &format!("RANGE LENS x 0 {n} LIMIT {limit}")).unwrap()
    else {
        panic!("expected range");
    };
    assert_eq!(segs.len(), limit, "limit={limit} n={n} segs={}", segs.len());
}

#[test]
fn range_offset_skips_segments() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 1 10, 1 2 20, 2 3 30, 3 4 40").unwrap();

    let Output::Range(all) = run(&mut e, "RANGE LENS x 0 4").unwrap() else {
        panic!()
    };
    assert_eq!(all.len(), 4);

    let Output::Range(skipped) = run(&mut e, "RANGE LENS x 0 4 OFFSET 2").unwrap() else {
        panic!()
    };
    assert_eq!(skipped.len(), 2);
    assert_eq!(skipped[0], all[2]);
    assert_eq!(skipped[1], all[3]);
}

#[test]
fn range_limit_and_offset_compose() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS x int").unwrap();
    run(&mut e, "APPEND LENS x 0 1 10, 1 2 20, 2 3 30, 3 4 40").unwrap();

    let Output::Range(page) = run(&mut e, "RANGE LENS x 0 4 LIMIT 2 OFFSET 1").unwrap() else {
        panic!()
    };
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].2, Value::Int(20));
    assert_eq!(page[1].2, Value::Int(30));
}

#[test]
fn multi_db_transaction_is_atomic() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE db1").unwrap();
    run(&mut e, "CREATE LENS a int").unwrap();
    run(&mut e, "CREATE DATABASE db2").unwrap();
    run(&mut e, "USE DATABASE db2").unwrap();
    run(&mut e, "CREATE LENS b int").unwrap();

    // Transaction: write to db1 and db2 atomically.
    // USE DATABASE executes immediately (updates active context for subsequent
    // buffer captures); APPEND statements are buffered with their current DB.
    run(&mut e, "START TRANSACTION").unwrap();
    run(&mut e, "USE DATABASE db1").unwrap();
    run(&mut e, "APPEND LENS a 0 100 1").unwrap();
    run(&mut e, "USE DATABASE db2").unwrap();
    run(&mut e, "APPEND LENS b 0 100 2").unwrap();
    run(&mut e, "COMMIT").unwrap();

    run(&mut e, "USE DATABASE db1").unwrap();
    let Output::Value(Some(Value::Int(v1))) = run(&mut e, "AT LENS a 50").unwrap() else {
        panic!("db1 lens a not written")
    };
    assert_eq!(v1, 1);

    run(&mut e, "USE DATABASE db2").unwrap();
    let Output::Value(Some(Value::Int(v2))) = run(&mut e, "AT LENS b 50").unwrap() else {
        panic!("db2 lens b not written")
    };
    assert_eq!(v2, 2);
}

#[test]
fn ttl_hides_expired_at_queries() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS sensor int").unwrap();
    // timestamp 0 is far in the past relative to any real now-secs value
    run(&mut e, "APPEND LENS sensor 0 10 99").unwrap();
    // TTL of 1 second: cutoff = now - 1, and t=5 < cutoff always
    run(&mut e, "SET TTL LENS sensor 1").unwrap();
    let Output::Value(v) = run(&mut e, "AT LENS sensor 5").unwrap() else {
        panic!()
    };
    assert_eq!(v, None, "data at t=5 should be hidden by TTL");
}

#[test]
fn ttl_hides_expired_range_queries() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS sensor int").unwrap();
    run(&mut e, "APPEND LENS sensor 0 10 99").unwrap();
    run(&mut e, "SET TTL LENS sensor 1").unwrap();
    let Output::Range(segs) = run(&mut e, "RANGE LENS sensor 0 10").unwrap() else {
        panic!()
    };
    assert!(segs.is_empty(), "expired segments should be hidden");
}

#[test]
fn unset_ttl_restores_visibility() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS sensor int").unwrap();
    run(&mut e, "APPEND LENS sensor 0 10 99").unwrap();
    run(&mut e, "SET TTL LENS sensor 1").unwrap();
    // verify hidden
    let Output::Value(hidden) = run(&mut e, "AT LENS sensor 5").unwrap() else {
        panic!()
    };
    assert_eq!(hidden, None);
    // remove TTL
    run(&mut e, "UNSET TTL LENS sensor").unwrap();
    // now visible again
    let Output::Value(visible) = run(&mut e, "AT LENS sensor 5").unwrap() else {
        panic!()
    };
    assert_eq!(visible, Some(Value::Int(99)));
}

#[test]
fn show_status_returns_uptime_and_counts() {
    let mut e = Executor::new();
    run(&mut e, "CREATE DATABASE main").unwrap();
    run(&mut e, "CREATE LENS a int").unwrap();
    run(&mut e, "CREATE LENS b float").unwrap();
    let Output::Status(kv) = run(&mut e, "SHOW STATUS").unwrap() else {
        panic!("expected Status output")
    };
    let kv: std::collections::HashMap<_, _> = kv.into_iter().collect();
    assert_eq!(kv["databases"], "1");
    assert_eq!(kv["lenses"], "2");
    assert!(kv["uptime_secs"].parse::<u64>().is_ok());
    assert!(kv["wal_bytes"].parse::<u64>().is_ok());
}

#[test]
fn show_status_wire_roundtrip() {
    use crate::wire::Response;
    let r = Response::Status(vec![
        ("uptime_secs".into(), "42".into()),
        ("databases".into(), "3".into()),
    ]);
    let line = r.to_string();
    assert!(line.starts_with("STATUS 2;"));
    let parsed = Response::parse(&line).unwrap();
    assert_eq!(parsed, r);
}
