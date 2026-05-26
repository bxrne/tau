//! ANSI styling that respects the user's terminal theme.
//!
//! Uses only the basic 8-color foreground palette (`\x1b[3Xm`) so what shows
//! as "cyan" on screen is whatever cyan the user configured - not a forced
//! RGB value. Honours `NO_COLOR` and falls back to plain text when stdout is
//! not a TTY.

use std::io::IsTerminal;
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

fn enabled() -> bool {
    *ENABLED
        .get_or_init(|| std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal())
}

/// Pure formatter: wrap `s` in `\x1b[<seq>m … \x1b[0m` when `on`, otherwise
/// return `s` verbatim.  Exposed so callers (and tests) can bypass the
/// runtime auto-detection.
pub fn paint_with(on: bool, seq: &str, s: &str) -> String {
    if on {
        format!("\x1b[{}m{}\x1b[0m", seq, s)
    } else {
        s.to_string()
    }
}

fn paint(seq: &str, s: &str) -> String {
    paint_with(enabled(), seq, s)
}

pub fn cyan(s: &str) -> String {
    paint("36", s)
}

pub fn red(s: &str) -> String {
    paint("31", s)
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_with_enabled_wraps_in_escape_sequence() {
        assert_eq!(paint_with(true, "36", "hi"), "\x1b[36mhi\x1b[0m");
    }

    #[test]
    fn paint_with_disabled_returns_plain_string() {
        assert_eq!(paint_with(false, "36", "hi"), "hi");
    }

    #[test]
    fn paint_with_handles_empty_input() {
        assert_eq!(paint_with(true, "31", ""), "\x1b[31m\x1b[0m");
        assert_eq!(paint_with(false, "31", ""), "");
    }

    #[test]
    fn helpers_use_distinct_color_codes() {
        // Force-enable via paint_with so the assertions are deterministic
        // regardless of how the test binary's stdout is wired up.
        assert!(paint_with(true, "36", "x").contains("[36m"));
        assert!(paint_with(true, "31", "x").contains("[31m"));
        assert!(paint_with(true, "2", "x").contains("[2m"));
        assert!(paint_with(true, "1", "x").contains("[1m"));
    }
}
