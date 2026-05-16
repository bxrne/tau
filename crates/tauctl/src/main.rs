//! tauctl - interactive TUI client for tau databases.

mod commands;
mod tcpmgr;
mod tui;

const TAU_SYMBOL: char = 'τ';

use clap::Parser;

/// Interactive TUI client for tau databases.
#[derive(Parser)]
#[command(name = "tauctl", author, version)]
#[command(about = "Interactive TUI client for tau databases", long_about = None)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
    if !tui::is_tty() {
        eprintln!("tauctl requires an interactive terminal");
        std::process::exit(1);
    }
    if let Err(e) = tui::run() {
        eprintln!("TUI error: {e}");
        std::process::exit(1);
    }
}
