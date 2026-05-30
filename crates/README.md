# crates

Tau is a Cargo workspace. One engine library, one shared test/bench harness, and three binaries.

| crate | kind | purpose |
|-------|------|---------|
| [`libtau`](libtau/src/README.md) | library | The engine: data model, query language, storage backends, executor. Everything else depends on it. |
| [`libharness`](libharness) | library | Shared simulation + benchmark harness: backend abstractions over the engine, the reference oracle, deterministic generators, and reporting. Consumed by `dst` and the Criterion benches. |
| [`tau`](tau/README.md) | binary | TCP server: exposes a `libtau` executor over a line-oriented protocol with optional TLS, auth, and WAL. |
| [`tauctl`](tauctl/README.md) | binary (`ctl`) | Interactive client: named connection pool, client-side CSV load. |
| [`dst`](dst/README.md) | binary | Deterministic simulation tester driven by the 1BRC dataset: cross-checks every result against the oracle, injects faults, and measures throughput. |

Binary names are unique across the workspace, so `cargo run --bin tau|ctl|dst` works from the repo root. See [`CONTRIBUTING.md`](../CONTRIBUTING.md) for build instructions.
