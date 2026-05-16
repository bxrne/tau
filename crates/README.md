# crates

Tau is a Cargo workspace. One engine library, two binaries, and one fuzz harness.

| crate | kind | purpose |
|-------|------|---------|
| [`libtau`](libtau/src/README.md) | library | The engine: data model, query language, storage backends, executor. Everything else depends on it. |
| [`tau`](tau/README.md) | binary | TCP server: exposes a `libtau` executor over a line-oriented protocol with optional TLS, auth, and WAL. |
| [`tauctl`](tauctl/README.md) | binary (`tauctl`) | Interactive client: named connection pool, client-side CSV load. |
| [`fuzztau`](fuzztau/README.md) | fuzz harness | LibFuzzer targets for `libtau::parse` and `libtau::Response::parse`; requires a nightly toolchain. |

Binary names are unique across the workspace, so `cargo run --bin tau|tauctl` works from the repo root. See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for build instructions.
