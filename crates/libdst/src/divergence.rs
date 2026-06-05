//! Structured divergence record produced when the SUT and reference model disagree.

/// A single mismatch between the system under test and the reference model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Step index at which the divergence occurred.
    pub step: usize,
    /// Human-readable description (e.g. `"AT lens a 100"`).
    pub description: String,
    /// `Debug` representation of the expected (model) value.
    pub expected: String,
    /// `Debug` representation of the actual (SUT) value.
    pub got: String,
}

impl Divergence {
    pub fn new(
        step: usize,
        description: impl Into<String>,
        expected: impl std::fmt::Debug,
        got: impl std::fmt::Debug,
    ) -> Self {
        Self {
            step,
            description: description.into(),
            expected: format!("{expected:?}"),
            got: format!("{got:?}"),
        }
    }
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "step={} {} expected={} got={}",
            self.step, self.description, self.expected, self.got
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_contains_all_fields() {
        let d = Divergence::new(5, "AT lens a 100", Some(42i64), None::<i64>);
        let s = d.to_string();
        assert!(s.contains("step=5"), "{s}");
        assert!(s.contains("AT lens a 100"), "{s}");
    }

    #[test]
    fn equality() {
        let d1 = Divergence::new(1, "x", 1, 2);
        let d2 = Divergence::new(1, "x", 1, 2);
        assert_eq!(d1, d2);
    }
}
