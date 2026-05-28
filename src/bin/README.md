# bin

Three binaries, each consuming `libtau` for a different purpose.

| binary | purpose |
|--------|---------|
| [`tau`](tau/README.md) | TCP server: exposes a `libtau` executor over a line-oriented protocol with optional TLS, auth, and WAL. |
| [`tauctl`](tauctl/README.md) | Interactive REPL: line editing, named connection pool, client-side CSV load. |
| [`dst`](dst/README.md) | Deterministic simulation tester: correctness verification and throughput measurement across all config combinations. |

See [`CONTRIBUTING.md`](../../CONTRIBUTING.md) for build instructions.
