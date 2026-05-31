# tauctl

Interactive client for tau databases. Defaults to a ratatui TUI when stdout is a TTY; use `--headless` for the rustyline line-editor REPL (auto-selected when piping or scripting).

## TUI mode

The default when stdout is a TTY. Layout:

```
┌─ Connections ──────┬─ Results ──────────────────────────┐
│ ▶ prod  7070  plain│  RANGE 3 segments                  │
│   local 7071  tls  │  start │ end  │ value              │
│                    │      0 │   10 │ f18.0              │
├────────────────────┴────────────────────────────────────┤
│ τ› (Enter to send, Ctrl-C to quit)                       │
├──────────────────────────────────────────────────────────┤
│ Log                                                      │
└──────────────────────────────────────────────────────────┘
```

Key bindings: `Enter` submits, `Ctrl-C` quits. All other keys are handled by tui-textarea (arrows, history, delete, etc.).

## Headless mode

```bash
ctl --headless              # rustyline REPL
ctl                         # auto-headless when stdout is not a TTY
```

## Built-in commands

| Command | Description |
|---------|-------------|
| `connect <name> <host:port> [tls] [<user> <pass>]` | Open a TCP connection |
| `disconnect <name>` | Close a connection |
| `use <name>` | Switch the active connection |
| `connections` | List all open connections |
| `auth <user> <pass>` | Send AUTH on the active connection |
| `load <lens> <local-path> [chunk]` | Ship a local CSV as batched APPENDs |
| `help` | Show built-in command list |
| `exit` / `quit` | Exit |

Any other input is forwarded as a TauQL statement to the active connection.

## Running

```bash
cargo run --release --bin ctl              # TUI (default on TTY)
cargo run --release --bin ctl -- --headless  # line-editor REPL
```
