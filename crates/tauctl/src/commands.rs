//! Shared bulk-load helpers used by the TUI I/O thread.

use crate::tcpmgr::Connection;

/// Parse one CSV data line (`start,end,value`) into a tau triple string.
/// Returns `None` for blank lines or `#` comments; returns `Err` for malformed lines.
pub(crate) fn parse_csv_line(
    raw: &str,
    path: &str,
    lineno: usize,
) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }
    let mut parts = trimmed.splitn(3, ',');
    let start = parts
        .next()
        .ok_or_else(|| format!("{}:{}: missing start", path, lineno + 1))?
        .trim();
    let end = parts
        .next()
        .ok_or_else(|| format!("{}:{}: missing end", path, lineno + 1))?
        .trim();
    let value = parts
        .next()
        .ok_or_else(|| format!("{}:{}: missing value", path, lineno + 1))?
        .trim();
    Ok(Some(format!("{} {} {}", start, end, value)))
}

/// Flush `buffer` to the server as one `APPEND` statement.  Updates `total`
/// and `chunks` on success; returns `Err` on server rejection or I/O failure.
pub(crate) fn flush_chunk(
    conn: &mut Connection,
    lens: &str,
    buffer: &mut Vec<String>,
    total: &mut u64,
    chunks: &mut u64,
) -> Result<(), String> {
    if buffer.is_empty() {
        return Ok(());
    }
    let resp = ship(conn, lens, buffer)?;
    if resp.is_err() {
        return Err(format!(
            "server rejected chunk #{} at row {}: {}",
            *chunks + 1,
            *total + buffer.len() as u64,
            resp
        ));
    }
    *total += buffer.len() as u64;
    *chunks += 1;
    buffer.clear();
    Ok(())
}

/// Build and send one `APPEND LENS <lens> s e v, s e v, ...` statement.
pub(crate) fn ship(
    conn: &mut Connection,
    lens: &str,
    taus: &[String],
) -> Result<libtau::Response, String> {
    let mut stmt = String::with_capacity(32 + taus.len() * 24);
    stmt.push_str("APPEND LENS ");
    stmt.push_str(lens);
    stmt.push(' ');
    for (i, t) in taus.iter().enumerate() {
        if i > 0 {
            stmt.push_str(", ");
        }
        stmt.push_str(t);
    }
    conn.send(&stmt).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn blank_and_comment_lines_return_none() {
        assert_eq!(parse_csv_line("", "f", 0).unwrap(), None);
        assert_eq!(parse_csv_line("   ", "f", 0).unwrap(), None);
        assert_eq!(parse_csv_line("# header", "f", 0).unwrap(), None);
    }

    #[test]
    fn valid_line_parses_to_triple() {
        assert_eq!(
            parse_csv_line("0,10,42", "f", 0).unwrap(),
            Some("0 10 42".to_string())
        );
    }

    #[test]
    fn whitespace_around_fields_is_stripped() {
        assert_eq!(
            parse_csv_line(" 0 , 10 , hello ", "f", 0).unwrap(),
            Some("0 10 hello".to_string())
        );
    }

    #[test]
    fn missing_fields_return_err() {
        assert!(parse_csv_line("0,10", "f", 0).is_err());
        assert!(parse_csv_line("0", "f", 0).is_err());
    }
}
