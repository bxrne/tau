# crates

## What it is

Tau is a Cargo workspace: one engine library, two runtime binaries, and three testing crates. Binary names are unique, so `cargo run --bin tau|tauctl|dst` works from the repo root.

## How the crates fit

`libtau` is the engine — a syscall-routing microkernel (`Kernel`) over db, query, auth, and metrics services. The `tau` server exposes a kernel over line-oriented TCP (optional TLS, auth, WAL); `tauctl` is the interactive TUI client speaking the same wire protocol. Neither binary contains engine logic.

`libdst` is a generic deterministic-simulation framework; `dst` is its Tau driver, driving a kernel and an independent oracle in lock-step. `fuzztau` holds LibFuzzer targets for the TauQL and wire parsers (nightly toolchain). None of the three are in the runtime dependency chain.

## Where to look

Each crate has its own README covering what it is, how it works, and how to use it: [libtau](libtau/README.md), [tau](tau/README.md), [tauctl](tauctl/README.md), [fuzztau](fuzztau/README.md).

