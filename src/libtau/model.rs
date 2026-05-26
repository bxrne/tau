use std::sync::Arc;

pub type Timestamp = i64;
pub type LayerId = u64;

/// An atomic temporal fact: value V is true over [start, end).
#[derive(Debug, Clone)]
pub struct Tau<V> {
    pub start: Timestamp,
    pub end: Timestamp,
    pub value: V,
}

impl<V> Tau<V> {
    pub fn new(start: Timestamp, end: Timestamp, value: V) -> Self {
        assert!(start < end, "Tau: start must be less than end");
        Self { start, end, value }
    }

    #[inline]
    pub fn contains(&self, t: Timestamp) -> bool {
        self.start <= t && t < self.end
    }
}

/// An append batch / revision: a sorted, non-overlapping slice of Tau<V>.
#[derive(Debug, Clone)]
pub struct Layer<V> {
    pub id: LayerId,
    pub taus: Arc<[Tau<V>]>,
}

impl<V> Layer<V> {
    pub fn new(id: LayerId, mut taus: Vec<Tau<V>>) -> Self {
        taus.sort_by_key(|t| t.start);
        assert!(
            taus.windows(2).all(|w| w[0].end <= w[1].start),
            "Layer: taus must not overlap"
        );
        Self {
            id,
            taus: taus.into(),
        }
    }

    /// O(log n) point lookup via binary search.
    pub fn at(&self, t: Timestamp) -> Option<&V> {
        let i = self.taus.partition_point(|tau| tau.end <= t);
        self.taus
            .get(i)
            .filter(|tau| tau.start <= t)
            .map(|tau| &tau.value)
    }
}

pub type Eval<V> = Arc<dyn Fn(Timestamp) -> Option<V> + Send + Sync>;

/// Discriminates base (storage-backed) from derived (computed) lenses.
pub enum LensKind<V> {
    Base,
    Derived(Eval<V>),
}

impl<V> Clone for LensKind<V> {
    fn clone(&self) -> Self {
        match self {
            LensKind::Base => LensKind::Base,
            LensKind::Derived(f) => LensKind::Derived(f.clone()),
        }
    }
}

/// A temporal function of time.
///
/// Base lenses read from the store; derived lenses compute lazily.
/// Cloning is cheap - a single Arc bump in either case.
#[derive(Clone)]
pub struct Lens<V> {
    pub name: Arc<str>,
    pub kind: LensKind<V>,
}

impl<V> Lens<V> {
    /// Evaluate a derived lens directly - no store needed.
    /// Base lenses always return `None` here; use `Database::at` for those.
    pub fn eval(&self, t: Timestamp) -> Option<V> {
        match &self.kind {
            LensKind::Derived(f) => f(t),
            LensKind::Base => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tau_new_stores_fields() {
        let t = Tau::new(0, 10, 42i32);
        assert_eq!(t.start, 0);
        assert_eq!(t.end, 10);
        assert_eq!(t.value, 42);
    }

    #[test]
    #[should_panic(expected = "Tau: start must be less than end")]
    fn tau_new_panics_when_start_equals_end() {
        Tau::new(5, 5, 0i32);
    }

    #[test]
    #[should_panic(expected = "Tau: start must be less than end")]
    fn tau_new_panics_when_start_exceeds_end() {
        Tau::new(10, 5, 0i32);
    }

    #[test]
    fn tau_contains_start_inclusive_end_exclusive() {
        let t = Tau::new(10, 20, ());
        assert!(!t.contains(9));
        assert!(t.contains(10)); // inclusive start
        assert!(t.contains(15));
        assert!(!t.contains(20)); // exclusive end
        assert!(!t.contains(21));
    }

    #[test]
    fn tau_clone_copies_value() {
        let a = Tau::new(0, 5, "hello");
        let b = a.clone();
        assert_eq!(b.start, 0);
        assert_eq!(b.end, 5);
        assert_eq!(b.value, "hello");
    }

    #[test]
    fn layer_sorts_taus_by_start() {
        let layer = Layer::new(
            1,
            vec![
                Tau::new(20, 30, 2i32),
                Tau::new(0, 10, 0i32),
                Tau::new(10, 20, 1i32),
            ],
        );
        assert_eq!(layer.taus[0].value, 0);
        assert_eq!(layer.taus[1].value, 1);
        assert_eq!(layer.taus[2].value, 2);
    }

    #[test]
    #[should_panic(expected = "Layer: taus must not overlap")]
    fn layer_panics_on_overlapping_taus() {
        Layer::new(1, vec![Tau::new(0, 15, 0i32), Tau::new(10, 20, 1i32)]);
    }

    #[test]
    fn layer_allows_adjacent_taus() {
        // end == next start is fine (contiguous, not overlapping)
        let layer = Layer::new(1, vec![Tau::new(0, 10, 0i32), Tau::new(10, 20, 1i32)]);
        assert_eq!(layer.taus.len(), 2);
    }

    #[test]
    fn layer_at_returns_value_for_covered_timestamp() {
        let layer = Layer::new(1, vec![Tau::new(0, 10, 100i32), Tau::new(20, 30, 200i32)]);
        assert_eq!(layer.at(5), Some(&100));
        assert_eq!(layer.at(25), Some(&200));
    }

    #[test]
    fn layer_at_returns_none_for_gap() {
        let layer = Layer::new(1, vec![Tau::new(0, 10, 1i32), Tau::new(20, 30, 2i32)]);
        assert_eq!(layer.at(10), None); // exclusive end
        assert_eq!(layer.at(15), None); // gap
        assert_eq!(layer.at(30), None); // exclusive end of last
    }

    #[test]
    fn layer_at_returns_none_when_empty() {
        let layer: Layer<i32> = Layer::new(1, vec![]);
        assert_eq!(layer.at(0), None);
    }

    #[test]
    fn layer_at_uses_binary_search_correctly_on_large_input() {
        // 1000 non-overlapping taus: [i*10, i*10+5) for i in 0..1000
        let taus: Vec<Tau<i32>> = (0..1000i32)
            .map(|i| Tau::new(i as i64 * 10, i as i64 * 10 + 5, i))
            .collect();
        let layer = Layer::new(1, taus);
        assert_eq!(layer.at(5002), Some(&500)); // inside [5000, 5005)
        assert_eq!(layer.at(5005), None); // gap [5005, 5010)
    }

    #[test]
    fn layer_clone_shares_arc() {
        let layer = Layer::new(1, vec![Tau::new(0, 10, 42i32)]);
        let clone = layer.clone();
        // Both point to the same allocation - same pointer.
        assert!(Arc::ptr_eq(&layer.taus, &clone.taus));
    }

    #[test]
    fn lens_base_eval_returns_none() {
        let lens: Lens<i32> = Lens {
            name: "x".into(),
            kind: LensKind::Base,
        };
        assert_eq!(lens.eval(0), None);
        assert_eq!(lens.eval(100), None);
    }

    #[test]
    fn lens_derived_eval_calls_closure() {
        let f: Eval<i32> = Arc::new(|t| Some(t as i32 * 2));
        let lens = Lens {
            name: "x".into(),
            kind: LensKind::Derived(f),
        };
        assert_eq!(lens.eval(5), Some(10));
        assert_eq!(lens.eval(0), Some(0));
    }

    #[test]
    fn lens_derived_eval_returns_none_when_closure_does() {
        let f: Eval<i32> = Arc::new(|_t| None);
        let lens = Lens {
            name: "x".into(),
            kind: LensKind::Derived(f),
        };
        assert_eq!(lens.eval(42), None);
    }

    #[test]
    fn lens_kind_base_clones_to_base() {
        let kind: LensKind<i32> = LensKind::Base;
        let cloned = kind.clone();
        assert!(matches!(cloned, LensKind::Base));
    }

    #[test]
    fn lens_kind_derived_clone_shares_arc() {
        let f: Eval<i32> = Arc::new(|_| Some(1));
        let kind = LensKind::Derived(f.clone());
        let cloned = kind.clone();
        if let LensKind::Derived(g) = cloned {
            // same underlying function pointer
            assert!(Arc::ptr_eq(&f, &g));
        } else {
            panic!("expected Derived");
        }
    }

    #[test]
    fn lens_clone_preserves_name_and_kind() {
        let lens: Lens<i32> = Lens {
            name: "sensor".into(),
            kind: LensKind::Base,
        };
        let clone = lens.clone();
        assert_eq!(clone.name.as_ref(), "sensor");
        assert!(matches!(clone.kind, LensKind::Base));
    }
}
