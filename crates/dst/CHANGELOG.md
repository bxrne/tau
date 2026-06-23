# Changelog

## [0.3.0](https://github.com/bxrne/tau/compare/dst-v0.2.0...dst-v0.3.0) (2026-06-23)


### Features

* **dst, libdst, libtau:** add disk, network and wal faults, fixed caught issues and updated docs ([4b5b458](https://github.com/bxrne/tau/commit/4b5b458f623fe6c7fd6ed15e00b96a13277be836))
* **libtau, tau, dst:** Add materialised lenses via XDERIVE, with optional range for it and non materialised lenses ([e67006d](https://github.com/bxrne/tau/commit/e67006dffd7f50d200d285c88c061430ab573c4b))


### Bug Fixes

* **dst, libtau:** Case division between client and server commands and tests for xderive ([d668c90](https://github.com/bxrne/tau/commit/d668c903a13d7066868c9f1eb1f1010fe25aa4a9))

## [0.2.0](https://github.com/bxrne/tau/compare/dst-v0.1.3...dst-v0.2.0) (2026-06-15)


### Features

* **bench, libtau:** add deterministic benchmark crate, docs, and capped Docker stack ([bce1ce5](https://github.com/bxrne/tau/commit/bce1ce5f38569d9453d9d412444d928dbe730129))

## [0.1.3](https://github.com/bxrne/tau/compare/dst-v0.1.2...dst-v0.1.3) (2026-06-14)


### Bug Fixes

* **libtau, dst, tau:** flush on compact, wal added to disk properly, fixed dst/pbt test, and docs ([ccdbb8b](https://github.com/bxrne/tau/commit/ccdbb8bd1d7fc95a92963c241362945ca9d9fe61))

## [0.1.2](https://github.com/bxrne/tau/compare/dst-v0.1.1...dst-v0.1.2) (2026-06-10)


### Bug Fixes

* **libtau, libdst, dst:** fixed compr lenses on restart, COPY errors out instead of killing thread, batch append now sorted ([dc54ee0](https://github.com/bxrne/tau/commit/dc54ee0c1d376b16163f04be405087a89c2d0e12))

## [0.1.1](https://github.com/bxrne/tau/compare/dst-v0.1.0...dst-v0.1.1) (2026-06-06)


### Bug Fixes

* remove unwraps and use better comments ([5ceee8a](https://github.com/bxrne/tau/commit/5ceee8afbc400b4d2fddbab1a6027406afb5951d))

## 0.1.0 (2026-06-05)


### Features

* **tau, libtau, libdst, dst:** behaviour tree (weighted) driven DST with reference oracle and docs, added tests ([ff1fc79](https://github.com/bxrne/tau/commit/ff1fc795b6aea8f327f641c4f4a6af2b53b8ee06))
