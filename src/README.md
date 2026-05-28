# src

Source root. One library crate (`libtau`) and three binaries (`tau`, `tauctl`, `dst`).

All three binaries depend only on `libtau`. Changes to one binary rarely require changes to another. Changes to `libtau` may require updates across all three.

- [`libtau/`](libtau/README.md): the core engine - model, storage, executor, query language, metrics, crypto, users
- [`bin/tau/`](bin/tau/README.md): TCP server
- [`bin/tauctl/`](bin/tauctl/README.md): interactive REPL
- [`bin/dst/`](bin/dst/README.md): deterministic simulation tester
