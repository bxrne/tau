//! ANSI styling that respects the user's terminal theme.
//!
//! Uses only the basic 8-color foreground palette (`\x1b[3Xm`) so what shows
//! as "cyan" on screen is whatever cyan the user configured - not a forced
//! RGB value. Honours `NO_COLOR` and falls back to plain text when stdout is
//! not a TTY.

use std::env;
use std::io::{self, IsTerminal};
use std::sync::OnceLock;

static ENABLED: OnceLock<bool> = OnceLock::new();

fn enabled() -> bool {
    *ENABLED.get_or_init(|| env::var_os("NO_COLOR").is_none() && io::stdout().is_terminal())
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
    use hegel::TestCase;
    use hegel::generators as gs;

    #[hegel::test]
    fn paint_with_disabled_is_identity(tc: TestCase) {
        let seq = tc.draw(gs::from_regex("[0-9]{1,3}").fullmatch(true));
        let s = tc.draw(gs::text().max_size(64));
        assert_eq!(paint_with(false, &seq, &s), s);
    }

    #[hegel::test]
    fn paint_with_enabled_wraps_in_csi(tc: TestCase) {
        let seq = tc.draw(gs::from_regex("[0-9]{1,3}").fullmatch(true));
        let s = tc.draw(gs::text().max_size(64));
        let painted = paint_with(true, &seq, &s);
        assert_eq!(painted, format!("\x1b[{seq}m{s}\x1b[0m"));
        // And the body is always recoverable by stripping the CSI/reset pair.
        let body = painted
            .strip_prefix(&format!("\x1b[{seq}m"))
            .and_then(|x| x.strip_suffix("\x1b[0m"))
            .unwrap();
        assert_eq!(body, s);
    }

    #[test]
    fn helpers_use_distinct_color_codes() {
        assert!(paint_with(true, "36", "x").contains("[36m"));
        assert!(paint_with(true, "31", "x").contains("[31m"));
        assert!(paint_with(true, "2", "x").contains("[2m"));
        assert!(paint_with(true, "1", "x").contains("[1m"));
    }
}
