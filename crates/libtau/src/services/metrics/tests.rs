#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;
    use pretty_assertions::assert_eq;

    #[test]
    fn counters_start_at_zero() {
        let m = Metrics::new();
        for op in &m.ops {
            assert_eq!(op.count.get(), 0);
        }
        assert_eq!(m.compactions.get(), 0);
        assert_eq!(m.connections_active.get(), 0);
    }

    #[hegel::test]
    fn pbt_record_op_increments_count_and_ns(tc: TestCase) {
        let samples = tc
            .draw(gs::vecs(gs::integers::<u64>().min_value(0).max_value(10_000_000).max_size(16));
        let m = Metrics::new();
        for &ns in &samples {
            m.record_op(Op::Append, ns);
        }
        let append = &m.ops[Op::Append as usize];
        assert_eq!(append.count.get(), samples.len() as u64);
        assert_eq!(append.ns.get(), samples.iter().sum::<u64>());
    }

    #[test]
    fn record_compaction_increments() {
        let m = Metrics::new();
        m.record_compaction();
        m.record_compaction();
        assert_eq!(m.compactions.get(), 2);
    }

    #[test]
    fn wal_write_latency_records() {
        let m = Metrics::new();
        m.record_wal_write(5_000);
        let text = m.prometheus_text();
        assert!(text.contains("tau_wal_write_duration_microseconds"));
    }

    #[test]
    fn per_db_counts_accumulate() {
        let m = Metrics::new();
        m.record_db_op("main", Op::Append);
        m.record_db_op("main", Op::Append);
        m.record_db_op("other", Op::At);
        let main_label = DbOpLabel {
            db: "main".into(),
            r#type: "append".into(),
        };
        let other_label = DbOpLabel {
            db: "other".into(),
            r#type: "at".into(),
        };
        assert_eq!(m.per_db.get_or_create(&main_label).get(), 2);
        assert_eq!(m.per_db.get_or_create(&other_label).get(), 1);
    }

    #[test]
    fn set_active_connections_stores_value() {
        let m = Metrics::new();
        m.set_active_connections(42);
        assert_eq!(m.connections_active.get(), 42);
    }

    #[hegel::test]
    fn pbt_prometheus_text_exposes_every_documented_family(tc: TestCase) {
        let _ = tc;
        let m = Metrics::new();
        m.record_op(Op::Append, 100);
        m.record_compaction();
        m.record_wal_write(1000);
        m.record_db_op("main", Op::At);
        let text = m.prometheus_text();
        for family in [
            "tau_statements",
            "tau_statement_duration_microseconds",
            "tau_compactions",
            "tau_wal_write_duration_microseconds",
            "tau_connections_active",
            "tau_db_statements",
            "tau_process_resident_bytes",
        ] {
            assert!(text.contains(family), "missing: {family}");
        }
    }

    #[hegel::test]
    fn pbt_prometheus_text_count_matches_counters(tc: TestCase) {
        let n = tc.draw(gs::integers::<u64>().min_value(0).max_value(20));
        let m = Metrics::new();
        for _ in 0..n {
            m.record_op(Op::Append, 100);
        }
        let text = m.prometheus_text();
        assert!(text.contains(&format!("tau_statements_total{{type=\"append\"}} {n}")));
    }

    #[test]
    fn all_13_op_labels_appear_in_output() {
        let m = Metrics::new();
        let text = m.prometheus_text();
        for label in OP_LABELS {
            assert!(
                text.contains(&format!("type=\"{label}\"")),
                "missing op: {label}"
            );
        }
    }
}