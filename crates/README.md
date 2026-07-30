# crates

## What it is

Tau is a Cargo workspace: one engine library, two runtime binaries, and a fuzz crate. Binary names are unique, so `cargo run --bin tau|tauctl` works from the repo root.

## How the crates fit

`libtau` is the engine — a syscall-routing microkernel (`Kernel`) over db, query, auth, and metrics services. The `tau` server exposes a kernel over line-oriented TCP (optional TLS, auth, WAL); `tauctl` is the interactive TUI client speaking the same wire protocol. Neither binary contains engine logic.

`fuzztau` holds LibFuzzer targets for the TauQL and wire parsers (nightly toolchain). It is not in the runtime dependency chain.

Deterministic chaos testing lives in the `dst/` directory — Lua scripts driven by the external [dstest](https://crates.io/crates/dstest) CLI, which injects faults (pause, kill, resource deprivation) into Docker containers and verifies resilience.

## Where to look

Each crate has its own README covering what it is, how it works, and how to use it: [libtau](libtau/README.md), [tau](tau/README.md), [tauctl](tauctl/README.md), [fuzztau](fuzztau/README.md).

