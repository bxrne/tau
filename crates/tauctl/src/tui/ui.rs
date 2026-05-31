//! TUI rendering: layout + widget draw.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Row, Table, TableState, Wrap},
};
use tui_textarea::TextArea;

use libtau::{Response, storage::Codec};

use super::app::App;

/// Draw the full TUI.
pub fn draw(f: &mut Frame, app: &App, input: &TextArea) {
    let area = f.area();

    // Top row: [Connections 28%] [Results 72%]
    // Middle: input box  (3 lines)
    // Bottom: log pane   (remainder)
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(3),
            Constraint::Length(8),
        ])
        .split(area);

    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
        .split(vchunks[0]);

    draw_connections(f, app, top_chunks[0]);
    draw_results(f, app, top_chunks[1]);
    draw_input(f, input, vchunks[1]);
    draw_log(f, app, vchunks[2]);
}

fn draw_connections(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .connections
        .iter()
        .map(|(name, addr, active, tls)| {
            let marker = if *active { "▶ " } else { "  " };
            let tag = if *tls { " tls" } else { "" };
            let style = if *active {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(format!("{marker}{name}  {addr}{tag}")).style(style)
        })
        .collect();

    let status_hint = if app.pending { " …" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Connections{status_hint} "));

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn draw_results(f: &mut Frame, app: &App, area: Rect) {
    match &app.last_response {
        None => {
            let p = Paragraph::new("no query yet")
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::ALL).title(" Results "));
            f.render_widget(p, area);
        }
        Some(Response::Range(segs)) => {
            let header = Row::new(vec!["start", "end", "value"])
                .style(Style::default().add_modifier(Modifier::BOLD));
            let rows: Vec<Row> = segs
                .iter()
                .map(|(s, e, v)| Row::new(vec![s.to_string(), e.to_string(), v.encode()]))
                .collect();
            let widths = [
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Fill(1),
            ];
            let title = format!(" Results — RANGE ({} segments) ", segs.len());
            let table = Table::new(rows, widths)
                .header(header)
                .block(Block::default().borders(Borders::ALL).title(title));
            f.render_stateful_widget(table, area, &mut TableState::default());
        }
        Some(Response::Names(names)) => {
            let items: Vec<ListItem> = names.iter().map(|n| ListItem::new(n.as_str())).collect();
            let title = format!(" Results — NAMES ({}) ", names.len());
            let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
            f.render_widget(list, area);
        }
        Some(other) => {
            let text = other.to_string();
            let style = if other.is_err() {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            let p = Paragraph::new(text)
                .style(style)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" Results "));
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
        .take(area.height as usize)
        .map(|e| {
            let style = if e.is_err {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let text = if e.query.is_empty() {
                e.response.clone()
            } else {
                format!("{} → {}", e.query, e.response)
            };
            ListItem::new(Line::from(vec![Span::styled(text, style)]))
        })
        .collect();

    let title = format!(" Log ({}) ", app.log.len());
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(list, area);
}

/// Build the styled input textarea widget.
pub fn build_input_area<'a>(prompt: &str) -> TextArea<'a> {
    let mut ta = TextArea::default();
    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {prompt} (Enter to send, Ctrl-C to quit) ")),
    );
    ta.set_style(Style::default().fg(Color::White));
    ta.set_cursor_line_style(Style::default());
    ta
}
