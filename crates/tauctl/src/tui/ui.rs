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

use super::app::App;

fn panel(title: impl std::fmt::Display, border_color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(format!(" {title} "))
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
    let border_color = if app.pending {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let items: Vec<ListItem> = app
        .connections
        .iter()
        .map(|(name, addr, active, tls)| {
            let tls_tag = if *tls {
                Span::styled(
                    " [tls]",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                )
            } else {
                Span::raw("")
            };
            if *active {
                ListItem::new(Line::from(vec![
                    Span::styled("▶ ", Style::default().fg(Color::Green)),
                    Span::styled(
                        name.as_str(),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {addr}"), Style::default().fg(Color::Green)),
                    tls_tag,
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{name}  {addr}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    tls_tag,
                ]))
            }
        })
        .collect();

    let list = List::new(items).block(panel("Connections", border_color));
    f.render_widget(list, area);
}

fn draw_results(f: &mut Frame, app: &App, area: Rect) {
    match &app.last_response {
        None => {
            let p = Paragraph::new("no query yet")
                .style(Style::default().fg(Color::DarkGray))
                .block(panel("Results", Color::DarkGray));
            f.render_widget(p, area);
        }
        Some(Response::Range(segs)) => {
            let header = Row::new(vec!["start", "end", "value"]).style(
                Style::default()
                    .fg(Color::Cyan)
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
                .block(panel(title, Color::DarkGray));
            f.render_stateful_widget(table, area, &mut TableState::default());
        }
        Some(Response::Names(names)) => {
            let items: Vec<ListItem> = names
                .iter()
                .map(|n| {
                    ListItem::new(Line::from(vec![
                        Span::styled("• ", Style::default().fg(Color::Cyan)),
                        Span::raw(n.as_str()),
                    ]))
                })
                .collect();
            let title = format!("Results  {} names", names.len());
            let list = List::new(items).block(panel(title, Color::DarkGray));
            f.render_widget(list, area);
        }
        Some(other) => {
            let (color, border_color) = if other.is_err() {
                (Color::Red, Color::Red)
            } else {
                (Color::Green, Color::DarkGray)
            };
            let p = Paragraph::new(other.to_string())
                .style(Style::default().fg(color))
                .wrap(Wrap { trim: false })
                .block(panel("Results", border_color));
            f.render_widget(p, area);
        }
    }
}

fn draw_input(f: &mut Frame, input: &TextArea, area: Rect) {
    f.render_widget(input, area);
}

fn draw_log(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .log
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
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

    let title = format!("Log  {}", app.log.len());
    let list = List::new(items).block(panel(title, Color::DarkGray));
    f.render_widget(list, area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let status_str = app.status.as_str();
    let (fg, text): (Color, &str) =
        if status_str.starts_with("ERR") || status_str.starts_with("I/O") {
            (Color::Red, status_str)
        } else if app.pending {
            (Color::Yellow, "pending…")
        } else if status_str == "OK" {
            (Color::Green, "OK")
        } else {
            (Color::DarkGray, status_str)
        };

    let hints = "Ctrl-C quit · Alt+↵ newline";
    let text_len = text.chars().count() + 1;
    let hints_len = hints.chars().count() + 1;
    let gap = (area.width as usize).saturating_sub(text_len + hints_len);

    let line = Line::from(vec![
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
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(" {prompt} ")),
    );
    ta.set_style(Style::default().fg(Color::Reset));
    ta.set_cursor_line_style(Style::default());
    ta
}
