# tauctl

Interactive TUI client for tau databases.

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
cargo run --release --bin tauctl
```

`tauctl --version` and `tauctl --help` are the only accepted flags.
