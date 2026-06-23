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

The screen has four panes: an input box plus three read-only panes — **Connections**, **Results**, and **Log** — each tagged with a lazygit-style number badge (`[1]`/`[2]`/`[3]`).

### Key bindings

| Key | Action |
|-----|--------|
| `Enter` | Submit the query |
| `↑` / `↓` | Navigate input history (in the input box) |
| `Alt`+`1`/`2`/`3` | Focus the Connections / Results / Log pane (works mid-edit) |
| `Esc` / `i` | Return focus to the input box |
| `j`/`k`, `↑`/`↓` | Move the selection / scroll within a focused pane |
| `Enter` (Connections) | Activate the highlighted connection (`use`) |
| `y` | Copy the focused pane to the clipboard |
| `Ctrl-Y` | Copy the Results pane from anywhere |
| paste | Bracketed paste lands in the input box |
| `Ctrl-C` | Quit |

Clipboard copy uses OSC 52, so it works over SSH and on terminals without a native clipboard binding. Parse failures from the server are shown as a column-anchored message (e.g. `parse error at column 4: near \`...\``) rather than a raw debug dump.

## Built-in commands

`tauctl` meta-commands are **lowercase** — this is what distinguishes them from UPPERCASE TauQL, which is forwarded to the server. For example, lowercase `use` switches the active connection here, while UPPERCASE `USE` switches the active database on the server.

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
