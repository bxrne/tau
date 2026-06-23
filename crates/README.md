# crates

Tau is a Cargo workspace. One engine library, two binaries, and one fuzz harness.

| crate | kind | purpose |
|-------|------|---------|
| [`libtau`](libtau/src/README.md) | library | The engine: data model, query language, storage backends, executor. Everything else depends on it. |
| [`tau`](tau/README.md) | binary | TCP server: exposes a `libtau` executor over a line-oriented protocol with optional TLS, auth, and WAL. |
| [`tauctl`](tauctl/README.md) | binary (`tauctl`) | Interactive client: named connection pool, lazygit-style pane navigation, clipboard copy/paste, client-side CSV load. |
| [`fuzztau`](fuzztau/README.md) | fuzz harness | LibFuzzer targets for the TauQL/wire text parsers and the binary `.dat` decoder (`disk_decode`); a TauQL dictionary and committed seeds; requires a nightly toolchain. |

Binary names are unique across the workspace, so `cargo run --bin tau|tauctl` works from the repo root.

`dst`, `libdst`, and `fuzztau` complete the picture for testing and tooling but are not part of the runtime dependency chain (see their own READMEs).