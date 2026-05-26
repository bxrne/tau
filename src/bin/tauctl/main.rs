//! tauctl - interactive REPL for tau databases.

mod commands;
mod repl;
mod style;
mod tcpmgr;

/// Greek τ - used as the prompt sigil.
pub const TAU_SYMBOL: char = 'τ';

fn main() {
    let mut registry = commands::Registry::new();
    registry.register(commands::help_command());
    registry.register(commands::clear_command());
    registry.register(commands::connect_command());
    registry.register(commands::disconnect_command());
    registry.register(commands::use_command());
    registry.register(commands::connections_command());
    registry.register(commands::auth_command());

    let mut repl = repl::Repl::new(format!("{}: ", TAU_SYMBOL));
    repl.run(&registry);
}
