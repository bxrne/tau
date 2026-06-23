//! Clipboard copy via OSC 52.
//!
//! OSC 52 asks the *terminal emulator* to set the system clipboard, so it works
//! without linking a platform clipboard library and — unlike `xclip`/`wl-copy`
//! — keeps working across an SSH session.  Supported by Alacritty, foot, kitty,
//! ghostty, WezTerm and tmux (with `set-clipboard on`).
//!
//! Paste is handled separately by the terminal's bracketed-paste support, which
//! arrives as a `crossterm` `Event::Paste`.

use std::io::{self, Write};

/// Base64-encode `bytes` (standard alphabet, padded).  Inlined to avoid pulling
/// a crate in just for a handful of clipboard writes.
fn base64(bytes: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHA[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHA[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[(n >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Build the OSC 52 escape sequence that sets the clipboard to `text`.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Copy `text` to the system clipboard via OSC 52, writing to the terminal on
/// stdout.  Returns the number of bytes copied on success.
pub fn copy(text: &str) -> io::Result<usize> {
    let mut out = io::stdout();
    out.write_all(osc52(text).as_bytes())?;
    out.flush()?;
    Ok(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_payload_in_escape() {
        let seq = osc52("hi");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        assert!(seq.contains("aGk=")); // base64("hi")
    }
}
