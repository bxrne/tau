//! tauctl - interactive client for tau databases.
//!
//! Default: launches a ratatui TUI when stdout is a TTY.
//! `--headless`: falls back to the rustyline line-editor REPL (also auto-selected
//! when stdout is not a TTY, so pipes and CI work without a flag).

mod commands;
mod repl;
mod style;
mod tcpmgr;
mod tui;

/// Greek tau — used as the prompt sigil.
pub const TAU_SYMBOL: char = 'τ';

/// Compile-time version, sourced from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("tau ctl {}", VERSION);
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("tau ctl {}", VERSION);
        println!("usage: ctl [--version] [--help] [--headless]");
        println!();
        println!("Options:");
        println!("  --headless   Use the rustyline REPL instead of the ratatui TUI.");
        println!("               Automatically selected when stdout is not a TTY.");
        println!();
        println!("In the TUI: Enter to send, Ctrl-C to quit.");
        println!("In headless mode: type `help` for built-in commands, `exit` to quit.");
        println!("Use `connect <name> <host:port> [tls] [<user> <pass>]` to open a session.");
        return;
    }

    let headless = args.iter().any(|a| a == "--headless") || !tui::is_tty();

    if headless {
        run_headless();
    } else {
        if let Err(e) = tui::run() {
            eprintln!("TUI error: {e}");
            std::process::exit(1);
        }
    }
}

fn run_headless() {
    let mut registry = commands::Registry::new();
    registry.register(commands::help_command());
    registry.register(commands::clear_command());
    registry.register(commands::connect_command());
    registry.register(commands::disconnect_command());
    registry.register(commands::use_command());
    registry.register(commands::connections_command());
    registry.register(commands::auth_command());
    registry.register(commands::load_command());

    println!(
        "tau ctl {} — type `help` for commands, `exit` to quit",
        VERSION
    );
    let mut repl = repl::Repl::new(format!("{}: ", TAU_SYMBOL));
    repl.run(&registry);
}
