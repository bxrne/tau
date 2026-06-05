# tauctl

Interactive TUI client for tau databases.

## Install

```bash
# Release binary (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tauctl-x86_64-linux -o tauctl
chmod +x tauctl && sudo mv tauctl /usr/local/bin/

# Via cargo install (builds from source)
cargo install --git https://github.com/bxrne/tau tauctl
```

## TUI

Launches a ratatui TUI when stdout is a TTY. Exits with an error if stdout is not a terminal.


Key bindings: `Enter` submits, `↑`/`↓` navigate history, `Ctrl-C` quits.

## Built-in commands

| Command | Description |
|---------|-------------|
| `connect <name> <host:port> [tls]` | Open a TCP connection |
| `disconnect <name>` | Close a connection |
| `use <name>` | Switch the active connection |
| `load <lens> <local-path> [chunk]` | Ship a local CSV as batched APPENDs |
| `exit` / `quit` | Exit |

Any other input is forwarded as a TauQL statement to the active connection.

## Running

```bash
tauctl

# From source (developer workflow)
cargo run --release --bin tauctl
```

`tauctl --version` and `tauctl --help` are the only accepted flags.
