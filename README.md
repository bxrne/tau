# Tau

[![CI](https://github.com/bxrne/tau/actions/workflows/ci.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/ci.yml)
[![Release](https://github.com/bxrne/tau/actions/workflows/release.yml/badge.svg)](https://github.com/bxrne/tau/actions/workflows/release.yml)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=bxrne_tau&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=bxrne_tau)

A time-series database built on immutable, layered temporal intervals. Data is
never corrected in place: a new layer is appended and the newest layer wins at
query time.

## Docs

Full documentation lives at https://tau.bxrne.com. Please use the docs site for
query language reference, server operation, security, storage backends, and the
`tauctl` REPL.

## Quick start

```bash
cargo run --release                           # in-memory, listens on 127.0.0.1:7070
cargo run --release -- --wal -w data.wal     # with WAL durability
```

Connect with any TCP client:

```
> CREATE DATABASE main
< OK
> CREATE LENS temp float
< OK
> APPEND LENS temp 0 50 18.5, 50 100 21.0
< OK
> AT LENS temp 25
< VAL f18.5
```

## License

Tau is distributed under the [PolyForm Noncommercial License 1.0.0](LICENSE).

**Permitted (no payment required):**

- Personal use, hobby projects, research, experimentation, study, and hobby projects.
- Use by charitable organisations, educational institutions, public research bodies, public safety / health agencies, environmental protection organisations, and government institutions.
- Self-hosting Tau for any of the above, including running the Docker image inside your own infrastructure.
- Modifying Tau and distributing the modified source so long as recipients receive the same licence terms.

**Not permitted without a separate commercial licence:**

- Any commercial purpose, including using Tau (or a derivative of it) as part of a paid product, paid service, or revenue-generating business activity.
- Hosting Tau as a managed service that you sell access to.
- Internal use by a for-profit company for production workloads.

If you need a commercial licence, open an issue or get in touch via the email associated with the repository owner. The default position is "no" unless we explicitly agree otherwise in writing.
