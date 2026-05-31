//! ratatui TUI for tauctl.
//!
//! Runs when stdout is a TTY (default). For scripts and pipes use `--headless`.
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

pub use app::App;
use ui::build_input_area;

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

fn event_loop<B: ratatui::backend::Backend>(terminal: &mut Terminal<B>) -> io::Result<()> {
    let mut app = App::new();
    let prompt = format!("{}›", crate::TAU_SYMBOL);
    let mut input = build_input_area(&prompt);

    loop {
        // Drain any I/O responses.
        app.drain();

        if app.should_quit {
            break;
        }

        terminal.draw(|f| ui::draw(f, &app, &input))?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    // Ctrl-C quits.
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        break;
                    }
                    // Enter submits.
                    if key.code == KeyCode::Enter
                        && !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::SHIFT)
                    {
                        let lines = input.lines().join(" ");
                        let line = lines.trim().to_string();
                        if !line.is_empty() {
                            // Push into log with empty response (filled in later).
                            app.log.push(app::LogEntry {
                                query: line.clone(),
                                response: String::new(),
                                is_err: false,
                            });
                            app.submit(line);
                            // Clear input.
                            input = build_input_area(&prompt);
                        }
                        continue;
                    }
                    // All other keys go to tui-textarea.
                    input.input(key);
                }
                Event::Resize(_, _) => {} // ratatui auto-handles resize on next draw
                _ => {}
            }
        }
    }

    Ok(())
}
