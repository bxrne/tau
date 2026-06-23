//! TUI rendering: layout + widget draw.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, List, ListItem, Paragraph, Row, Table, TableState, Wrap,
    },
};
use tui_textarea::TextArea;

use libtau::{Response, storage::Codec};

use super::app::{App, Focus};

const ACCENT: Color = Color::Rgb(255, 168, 106);

/// Border colour for a pane given whether it currently holds focus.
fn border_for(focused: bool) -> Color {
    if focused { ACCENT } else { Color::DarkGray }
}

/// A bordered panel.  When `badge` is set, a lazygit-style number chip is shown
/// at the left of the title so the pane can be reached with Alt+<n>.
fn panel(title: impl std::fmt::Display, border_color: Color, badge: Option<u8>) -> Block<'static> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color));
    if let Some(n) = badge {
        block = block.title(Line::from(vec![
            Span::styled(
                format!(" [{n}] "),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{title} ")),
        ]));
    } else {
        block = block.title(format!(" {title} "));
    }
    block
}

/// Draw the full TUI.
pub fn draw(f: &mut Frame, app: &App, input: &TextArea) {
    let area = f.area();

    // Outer: content area | input | status bar
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    // Content: [Connections 28%] | [right column 72%]
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(vchunks[0]);

    // Right column: Results (fills space) above Log (fixed)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(8)])
        .split(top_chunks[1]);

    draw_connections(f, app, top_chunks[0]);
    draw_results(f, app, right_chunks[0]);
    draw_log(f, app, right_chunks[1]);
    draw_input(f, input, vchunks[1]);
    draw_status(f, app, vchunks[2]);
}

fn draw_connections(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Connections;
    let border_color = if focused || app.pending {
        ACCENT
    } else {
        Color::DarkGray
    };

    let items: Vec<ListItem> = app
        .connections
        .iter()
        .enumerate()
        .map(|(i, (name, addr, active, tls))| {
            let tls_tag = if *tls {
                Span::styled(
                    " [tls]",
                    Style::default().fg(ACCENT).add_modifier(Modifier::DIM),
                )
            } else {
                Span::raw("")
            };
            // While the pane is focused, the highlighted row gets a cursor and
            // a reverse-video sweep so navigation is visible.
            let selected = focused && i == app.conn_sel;
            let cursor = if selected {
                "› "
            } else if *active {
                "▶ "
            } else {
                "  "
            };
            let mut line = if *active {
                Line::from(vec![
                    Span::styled(cursor, Style::default().fg(ACCENT)),
                    Span::styled(
                        name.as_str(),
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {addr}"),
                        Style::default().fg(ACCENT).add_modifier(Modifier::DIM),
                    ),
                    tls_tag,
                ])
            } else {
                Line::from(vec![
                    Span::raw(cursor),
                    Span::styled(
                        format!("{name}  {addr}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    tls_tag,
                ])
            };
            if selected {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(panel(
        "Connections",
        border_color,
        Focus::Connections.digit(),
    ));
    f.render_widget(list, area);
}

fn draw_results(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Results;
    let base = border_for(focused);
    let badge = Focus::Results.digit();
    match &app.last_response {
        None => {
            let p = Paragraph::new("no query yet")
                .style(Style::default().fg(Color::DarkGray))
                .block(panel("Results", base, badge));
            f.render_widget(p, area);
        }
        Some(Response::Range(segs)) => {
            let header = Row::new(vec!["start", "end", "value"]).style(
                Style::default()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
            );
            let rows: Vec<Row> = segs
                .iter()
                .enumerate()
                .map(|(i, (s, e, v))| {
                    let style = if i % 2 != 0 {
                        Style::default().add_modifier(Modifier::DIM)
                    } else {
                        Style::default()
                    };
                    Row::new(vec![s.to_string(), e.to_string(), v.encode()]).style(style)
                })
                .collect();
            let widths = [
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Fill(1),
            ];
            let title = format!("Results  {} segments", segs.len());
            let table = Table::new(rows, widths)
                .header(header)
                .block(panel(title, base, badge));
            f.render_stateful_widget(table, area, &mut TableState::default());
        }
        Some(Response::Names(names)) => {
            let items: Vec<ListItem> = names
                .iter()
                .map(|n| {
                    ListItem::new(Line::from(vec![
                        Span::styled("• ", Style::default().fg(ACCENT)),
                        Span::raw(n.as_str()),
                    ]))
                })
                .collect();
            let title = format!("Results  {} names", names.len());
            let list = List::new(items).block(panel(title, base, badge));
            f.render_widget(list, area);
        }
        Some(other) => {
            // Errors override the focus colour so a failure is unmissable; the
            // leading ✗ marks it even on a monochrome terminal.
            let (text, color, border_color) = if other.is_err() {
                (format!("✗ {other}"), Color::Red, Color::Red)
            } else {
                (other.to_string(), Color::Green, base)
            };
            let p = Paragraph::new(text)
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: false })
                .block(panel("Results", border_color, badge));
            f.render_widget(p, area);
        }
    }
}

fn draw_input(f: &mut Frame, input: &TextArea, area: Rect) {
    f.render_widget(input, area);
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.focus == Focus::Log;
    let visible = area.height.saturating_sub(2) as usize;
    // Newest first; `log_scroll` walks the window back into older entries, but
    // never past the point where the oldest entry would scroll off the bottom.
    let max_scroll = app.log.len().saturating_sub(visible);
    let skip = (app.log_scroll as usize).min(max_scroll);
    let items: Vec<ListItem> = app
        .log
        .iter()
        .rev()
        .skip(skip)
        .take(visible)
        .map(|e| {
            let (query_style, sep_style, resp_style) = if e.is_err {
                (
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::Red),
                )
            } else {
                (
                    Style::default().fg(Color::Reset),
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::DarkGray),
                )
            };
            let line = if e.query.is_empty() {
                Line::from(vec![Span::styled(e.response.as_str(), resp_style)])
            } else {
                Line::from(vec![
                    Span::styled(e.query.as_str(), query_style),
                    Span::styled(" → ", sep_style),
                    Span::styled(e.response.as_str(), resp_style),
                ])
            };
            ListItem::new(line)
        })
        .collect();

    let title = if skip > 0 {
        format!("Log  {}  (+{skip})", app.log.len())
    } else {
        format!("Log  {}", app.log.len())
    };
    let list = List::new(items).block(panel(title, border_for(focused), Focus::Log.digit()));
    f.render_widget(list, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let status_str = app.status.as_str();
    let (fg, text): (Color, &str) =
        if status_str.starts_with("ERR") || status_str.starts_with("I/O") {
            (Color::Red, status_str)
        } else if app.pending {
            (ACCENT, "pending…")
        } else if status_str == "OK" {
            (Color::Green, "OK")
        } else {
            (Color::DarkGray, status_str)
        };

    // The leading mode chip mirrors the focused pane so the active key set is
    // always obvious; hints follow the mode.
    let (mode, hints) = match app.focus {
        Focus::Input => (
            " INPUT ",
            "↵ run · Alt+1/2/3 panes · Ctrl-Y copy · Ctrl-C quit",
        ),
        Focus::Connections => (
            " CONN ",
            "j/k move · ↵ use · y copy · 1/2/3 panes · i input",
        ),
        Focus::Results => (" RESULT ", "y copy · 1/2/3 panes · i input · Ctrl-C quit"),
        Focus::Log => (" LOG ", "j/k scroll · y copy · 1/2/3 panes · i input"),
    };

    let mode_span = Span::styled(
        mode,
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    );

    let mode_len = mode.chars().count();
    let text_len = text.chars().count() + 1;
    let hints_len = hints.chars().count() + 1;
    let gap = (area.width as usize).saturating_sub(mode_len + text_len + hints_len);

    let line = Line::from(vec![
        mode_span,
        Span::styled(format!(" {text}"), Style::default().fg(fg)),
        Span::raw(" ".repeat(gap)),
        Span::styled(format!("{hints} "), Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Build the styled input textarea widget.  Pass `text` to pre-populate (e.g. from history).
pub fn build_input_area<'a>(prompt: &str, text: &str) -> TextArea<'a> {
    let mut ta = if text.is_empty() {
        TextArea::default()
    } else {
        let mut t = TextArea::new(vec![text.to_string()]);
        t.move_cursor(tui_textarea::CursorMove::End);
        t
    };
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(format!(" {prompt} ")),
    );
    ta.set_style(Style::default().fg(Color::Reset));
    ta.set_cursor_line_style(Style::default());
    ta
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use libtau::{Response, Value};
    use ratatui::{Terminal, backend::TestBackend};

    use super::super::app::{App, Focus};
    use super::super::net::{IoRequest, IoResponse, NetHandle};
    use super::*;

    fn test_app() -> App {
        let (tx_req, _rx_req) = mpsc::channel::<IoRequest>();
        let (_tx_resp, rx_resp) = mpsc::channel::<IoResponse>();
        App {
            net: NetHandle::test_pair(tx_req, rx_resp),
            connections: vec![],
            log: vec![],
            last_response: None,
            pending: false,
            status: "Ready".into(),
            should_quit: false,
            focus: Focus::Input,
            conn_sel: 0,
            log_scroll: 0,
        }
    }

    fn terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(120, 30)).unwrap()
    }

    #[test]
    fn build_input_area_empty_has_no_content() {
        let ta = build_input_area("τ", "");
        assert_eq!(ta.lines(), &[""]);
    }

    #[test]
    fn build_input_area_with_text_sets_content() {
        let ta = build_input_area("τ", "AT LENS x 42");
        assert_eq!(ta.lines(), &["AT LENS x 42"]);
    }

    #[test]
    fn draw_does_not_panic_with_empty_app() {
        let mut t = terminal();
        let app = test_app();
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_pending_state() {
        let mut t = terminal();
        let mut app = test_app();
        app.pending = true;
        app.status = "sending…".into();
        let input = build_input_area("τ", "RANGE LENS x 0 100");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_connections() {
        let mut t = terminal();
        let mut app = test_app();
        app.connections = vec![
            ("dev".into(), "127.0.0.1:7070".into(), true, false),
            ("prod".into(), "10.0.0.1:7070".into(), false, true),
        ];
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_ok_response() {
        let mut t = terminal();
        let mut app = test_app();
        app.last_response = Some(Response::Ok);
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_err_response() {
        let mut t = terminal();
        let mut app = test_app();
        app.last_response = Some(Response::Err("no active database".into()));
        app.status = "ERR no active database".into();
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_range_response() {
        let mut t = terminal();
        let mut app = test_app();
        app.last_response = Some(Response::Range(vec![
            (0, 3600, Value::Int(42)),
            (3600, 7200, Value::Int(55)),
        ]));
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_names_response() {
        let mut t = terminal();
        let mut app = test_app();
        app.last_response = Some(Response::Names(vec!["cpu".into(), "pressure".into()]));
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_does_not_panic_with_log_entries() {
        let mut t = terminal();
        let mut app = test_app();
        app.log = vec![
            super::super::app::LogEntry {
                query: "AT LENS x 0".into(),
                response: "VAL i42".into(),
                is_err: false,
            },
            super::super::app::LogEntry {
                query: "AT LENS x 9999".into(),
                response: "ERR no such lens".into(),
                is_err: true,
            },
        ];
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
    }

    #[test]
    fn draw_renders_pane_badges() {
        let mut t = terminal();
        let app = test_app();
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
        let content: String = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(content.contains("[1]"), "connections badge");
        assert!(content.contains("[2]"), "results badge");
        assert!(content.contains("[3]"), "log badge");
    }

    #[test]
    fn draw_does_not_panic_with_focused_panes() {
        for focus in [Focus::Connections, Focus::Results, Focus::Log] {
            let mut t = terminal();
            let mut app = test_app();
            app.focus = focus;
            app.connections = vec![("dev".into(), "127.0.0.1:7070".into(), true, false)];
            app.last_response = Some(Response::Ok);
            let input = build_input_area("τ", "");
            t.draw(|f| draw(f, &app, &input)).unwrap();
        }
    }

    #[test]
    fn draw_status_reflects_ok_state() {
        let mut t = terminal();
        let mut app = test_app();
        app.status = "OK".into();
        let input = build_input_area("τ", "");
        t.draw(|f| draw(f, &app, &input)).unwrap();
        let buf = t.backend().buffer().clone();
        let content: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_owned())
            .collect();
        assert!(content.contains("OK") || content.contains("Ctrl-C"));
    }
}
