//! ratatui TUI for tauctl.
//!
//! Requires an interactive terminal; `main` exits early when stdout is not a TTY.
//!
//! Layout:
//!   ┌─ Connections ─┬─ Results ──────────────────┐
//!   │               │                            │
//!   ├───────────────┴────────────────────────────┤
//!   │ τ› input box                    (Enter ⏎)  │
//!   ├────────────────────────────────────────────┤
//!   │ Log                                        │
//!   └────────────────────────────────────────────┘
//!
//! Key bindings:
//!   Enter     — submit query
//!   Ctrl-C    — quit
//!   All other keys — tui-textarea handles them (arrows, history, delete, etc.)

mod app;
mod net;
mod ui;

use std::io::{self, IsTerminal, stdout};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;
use ui::build_input_area;

struct InputHistory {
    entries: Vec<String>,
    idx: Option<usize>,
    draft: String,
}

impl InputHistory {
    fn new() -> Self {
        Self {
            entries: vec![],
            idx: None,
            draft: String::new(),
        }
    }

    fn push(&mut self, line: String) {
        if self.entries.last().map(String::as_str) != Some(line.as_str()) {
            self.entries.push(line);
        }
        self.idx = None;
        self.draft = String::new();
    }

    fn up(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        match self.idx {
            None => {
                self.draft = current.to_string();
                self.idx = Some(self.entries.len() - 1);
            }
            Some(0) => {}
            Some(i) => {
                self.idx = Some(i - 1);
            }
        }
        Some(self.entries[self.idx.unwrap()].clone())
    }

    fn down(&mut self) -> Option<String> {
        match self.idx {
            None => None,
            Some(i) if i + 1 >= self.entries.len() => {
                self.idx = None;
                Some(self.draft.clone())
            }
            Some(i) => {
                self.idx = Some(i + 1);
                Some(self.entries[i + 1].clone())
            }
        }
    }
}

/// Returns `true` when stdout is an interactive TTY.
pub fn is_tty() -> bool {
    stdout().is_terminal()
}

/// Run the ratatui TUI.  Returns when the user quits.
pub fn run() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Panic hook: restore terminal before printing the backtrace.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = event_loop(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// Returns true if the event loop should quit.
fn handle_key(
    key: event::KeyEvent,
    app: &mut App,
    input: &mut tui_textarea::TextArea<'_>,
    hist: &mut InputHistory,
    prompt: &str,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    if key.code == KeyCode::Enter
        && !key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
    {
        let lines = input.lines().join(" ");
        let line = lines.trim().to_string();
        if !line.is_empty() {
            app.log.push(app::LogEntry {
                query: line.clone(),
                response: String::new(),
                is_err: false,
            });
            hist.push(line.clone());
            app.submit(line);
            *input = build_input_area(prompt, "");
        }
        return false;
    }
    // Up at first row: go back in history.
    if key.code == KeyCode::Up && key.modifiers.is_empty() && input.cursor().0 == 0 {
        let current = input.lines().join("\n");
        if let Some(text) = hist.up(&current) {
            *input = build_input_area(prompt, &text);
        }
        return false;
    }
    // Down at last row: go forward in history (or restore draft).
    if key.code == KeyCode::Down && key.modifiers.is_empty() {
        let last_row = input.lines().len().saturating_sub(1);
        if input.cursor().0 == last_row {
            let text = hist.down().unwrap_or_default();
            *input = build_input_area(prompt, &text);
            return false;
        }
    }
    input.input(key);
    false
}

fn event_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let mut app = App::new();
    let prompt = crate::TAU_SYMBOL.to_string();
    let mut input = build_input_area(&prompt, "");
    let mut hist = InputHistory::new();

    loop {
        app.drain();
        if app.should_quit {
            break;
        }
        terminal.draw(|f| ui::draw(f, &app, &input))?;
        if event::poll(Duration::from_millis(16))?
            && let Event::Key(key) = event::read()?
            && handle_key(key, &mut app, &mut input, &mut hist, &prompt)
        {
            break;
        }
    }

    Ok(())
}
