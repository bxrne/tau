//! Tau TCP server binary.

use clap::Parser;

fn main() -> std::io::Result<()> {
    tau::run_server(tau::Cli::parse())
}
