//! tauctl - interactive REPL for tau databases.

mod commands;
mod repl;
mod style;
mod tcpmgr;

/// Greek tau - used as the prompt sigil.
pub const TAU_SYMBOL: char = 'τ';

/// Compile-time version, sourced from Cargo.toml.  Bumped automatically by
/// release-please when a new tag is published, so the banner here always
/// matches the binary's git tag.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    // Hand-rolled --version / --help so we don't drag clap into the REPL binary.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("tau ctl {}", VERSION);
        return;
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("tau ctl {}", VERSION);
        println!("usage: ctl [--version] [--help]");
        println!();
        println!("Interactive REPL for tau databases.  Once running, type `help`");
        println!("for the built-in command list.  Use `connect <name> <host:port>`");
        println!("(optionally followed by `tls` and `<user> <pass>`) to open a session.");
        return;
    }

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
        "tau ctl {} - type `help` for commands, `exit` to quit",
        VERSION
    );
    let mut repl = repl::Repl::new(format!("{}: ", TAU_SYMBOL));
    repl.run(&registry);
}
