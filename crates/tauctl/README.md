# tauctl

## What it is

The interactive TUI client for tau databases. It launches a ratatui interface when stdout is a TTY (and errors otherwise): an input box plus three read-only panes — **Connections**, **Results**, and **Log** — each with a lazygit-style number badge.

## How it works

Meta-commands are **lowercase** — that is what distinguishes them from UPPERCASE TauQL, which is forwarded verbatim to the active connection. So lowercase `use` switches the active *connection* in the client, while UPPERCASE `USE` switches the active *database* on the server. The commands: `connect <name> <host:port> [tls]`, `disconnect <name>`, `use <name>`, `load <lens> <local-path> [chunk]` (ships a local CSV as batched APPENDs), and `exit`/`quit`.

Clipboard copy uses OSC 52, so it works over SSH and on terminals without a native clipboard binding. Server parse failures render as column-anchored messages rather than raw debug dumps.

## Using it

```bash
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tauctl-x86_64-linux -o tauctl
chmod +x tauctl && sudo mv tauctl /usr/local/bin/
cargo install --git https://github.com/bxrne/tau tauctl   # from source

tauctl        # --version / --help are the only flags
```

Keys: `Enter` submits; `↑`/`↓` walk input history; `Alt`+`1`/`2`/`3` focus a pane mid-edit; `Esc`/`i` return to the input box; `j`/`k` move within a focused pane; `Enter` on a connection activates it; `y` copies the focused pane (`Ctrl-Y` copies Results from anywhere); bracketed paste lands in the input box; `Ctrl-C` quits.
