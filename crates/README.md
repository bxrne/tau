# crates

Tau is a Cargo workspace. One engine library, and two binaries.

| crate | kind | purpose |
|-------|------|---------|
| [`libtau`](libtau/src/README.md) | library | The engine: data model, query language, storage backends, executor. Everything else depends on it. |
| [`tau`](tau/README.md) | binary | TCP server: exposes a `libtau` executor over a line-oriented protocol with optional TLS, auth, and WAL. |
| [`tauctl`](tauctl/README.md) | binary (`ctl`) | Interactive client: named connection pool, client-side CSV load. |

Binary names are unique across the workspace, so `cargo run --bin tau|ctl` works from the repo root. See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for build instructions.
