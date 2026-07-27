# Changelog

## [0.9.0](https://github.com/bxrne/tau/compare/libtau-v0.8.0...libtau-v0.9.0) (2026-07-27)


### Features

* **libtau:** instroducing func calls to lua from tauql ([54bcad0](https://github.com/bxrne/tau/commit/54bcad02aef18709563936e1144e8b5eb693ff7a))

## [0.8.0](https://github.com/bxrne/tau/compare/libtau-v0.7.0...libtau-v0.8.0) (2026-07-18)


### Features

* **libtau, fuzztau:** compaction now per-lens and configurable via TauQL also ([1b1a70a](https://github.com/bxrne/tau/commit/1b1a70a14a9db2f45cd0c4c67abe3b158ad375ed))

## [0.7.0](https://github.com/bxrne/tau/compare/libtau-v0.6.0...libtau-v0.7.0) (2026-07-07)


### Features

* **dst, libdst, libtau, tau, tauctl:** microkernel architecture refactoring and dst redesign, fuzzing improvements ([c3aac46](https://github.com/bxrne/tau/commit/c3aac4608d1155679caa76bbdfa8780d538e6fcc))

## [0.6.0](https://github.com/bxrne/tau/compare/libtau-v0.5.0...libtau-v0.6.0) (2026-07-03)


### Features

* **libtau, dst:** N-dimensional lenses via AXES ([e660311](https://github.com/bxrne/tau/commit/e660311ba723a50839e0af0c6164dbc609db98fc))
* **libtau:** Arc&lt;[Layer]&gt; snapshots for clone-free reads ([ad8964d](https://github.com/bxrne/tau/commit/ad8964d2989f78ed43e779abfcc4ff958ee6c1c0))
* **libtau:** lossless compaction for N-dimensional lenses ([e26a521](https://github.com/bxrne/tau/commit/e26a521cfd24c206212a34250baffd5e46b35344))
* **libtau:** preserve tx-generations through compaction ([aeb3552](https://github.com/bxrne/tau/commit/aeb35526a29129ace2e43d679de34b3d9ffc2ed3))
* **libtau:** Sstable backend (immutable runs + manifest, read-time MVCC) ([02fa68c](https://github.com/bxrne/tau/commit/02fa68cffa81802b3766ef322b2837c13a29538a))


### Bug Fixes

* **libtau, dst:** close two Sstable REDUCE-correctness gaps, cut Sstable DST profile in ([27eee81](https://github.com/bxrne/tau/commit/27eee81e4e3f8e66549e021d70c9c16ebb44fe12))
* **libtau, dst:** replace Sstable's three overlapping compaction triggers with one ([7fe2ed4](https://github.com/bxrne/tau/commit/7fe2ed4bba9635f36f2a71ebeb45a70d1722cadb))
* **libtau:** compile the n-orthotope Tau representation ([dbadb19](https://github.com/bxrne/tau/commit/dbadb19fa298ff968dc9f7673e8c34b6a1835b4e))

## [0.5.0](https://github.com/bxrne/tau/compare/libtau-v0.4.1...libtau-v0.5.0) (2026-06-23)


### Features

* **dst, libdst, libtau:** add disk, network and wal faults, fixed caught issues and updated docs ([4b5b458](https://github.com/bxrne/tau/commit/4b5b458f623fe6c7fd6ed15e00b96a13277be836))
* **libtau, tau, dst:** Add materialised lenses via XDERIVE, with optional range for it and non materialised lenses ([e67006d](https://github.com/bxrne/tau/commit/e67006dffd7f50d200d285c88c061430ab573c4b))


### Bug Fixes

* **dst, libtau:** Case division between client and server commands and tests for xderive ([d668c90](https://github.com/bxrne/tau/commit/d668c903a13d7066868c9f1eb1f1010fe25aa4a9))
* **fuzztau, libtau, tauctl:** added disk decoder to fuzzer, repaired emerging bugs ([261e524](https://github.com/bxrne/tau/commit/261e524b3dd0960df10557c8633bcb3df417fb24))

## [0.4.1](https://github.com/bxrne/tau/compare/libtau-v0.4.0...libtau-v0.4.1) (2026-06-15)


### Performance Improvements

* **bench, libtau:** added compaction on cap and fixed flush invocation ([70746bf](https://github.com/bxrne/tau/commit/70746bf464cbcce3e5ae1a345751cf03884f904c))

## [0.4.0](https://github.com/bxrne/tau/compare/libtau-v0.3.4...libtau-v0.4.0) (2026-06-15)


### Features

* **bench, libtau:** add deterministic benchmark crate, docs, and capped Docker stack ([bce1ce5](https://github.com/bxrne/tau/commit/bce1ce5f38569d9453d9d412444d928dbe730129))

## [0.3.4](https://github.com/bxrne/tau/compare/libtau-v0.3.3...libtau-v0.3.4) (2026-06-14)


### Bug Fixes

* **libtau, dst, tau:** flush on compact, wal added to disk properly, fixed dst/pbt test, and docs ([ccdbb8b](https://github.com/bxrne/tau/commit/ccdbb8bd1d7fc95a92963c241362945ca9d9fe61))

## [0.3.3](https://github.com/bxrne/tau/compare/libtau-v0.3.2...libtau-v0.3.3) (2026-06-10)


### Bug Fixes

* **libtau, tau:** bound wire line length, WAL serial append and ckpt for safe rotation w.r.t layers and added multi-stmt protocol tests ([a10d8a0](https://github.com/bxrne/tau/commit/a10d8a0f3d9465456a1078e23edb34d31fa3ca74))

## [0.3.2](https://github.com/bxrne/tau/compare/libtau-v0.3.1...libtau-v0.3.2) (2026-06-10)


### Bug Fixes

* **libtau, libdst, dst:** fixed compr lenses on restart, COPY errors out instead of killing thread, batch append now sorted ([dc54ee0](https://github.com/bxrne/tau/commit/dc54ee0c1d376b16163f04be405087a89c2d0e12))
* **libtau:** drop support for legacy fileformats and updated docs ([7e8f454](https://github.com/bxrne/tau/commit/7e8f4544e25a89a176548e0691d12de332a14213))
* **libtau:** update tauql docs and remove legacy WAL handling ([e7ccba2](https://github.com/bxrne/tau/commit/e7ccba22dd8933afc6f5ee21face93772d5a6fe4))

## [0.3.1](https://github.com/bxrne/tau/compare/libtau-v0.3.0...libtau-v0.3.1) (2026-06-06)


### Bug Fixes

* remove unwraps and use better comments ([5ceee8a](https://github.com/bxrne/tau/commit/5ceee8afbc400b4d2fddbab1a6027406afb5951d))

## [0.3.0](https://github.com/bxrne/tau/compare/libtau-v0.2.0...libtau-v0.3.0) (2026-06-06)


### Features

* **tau, libtau:** fix outdated doc, add disk ddl persistence (full) ([42b22b6](https://github.com/bxrne/tau/commit/42b22b62b1e3fe8cb5d98a4eb1fc167faa0ef224))


### Bug Fixes

* **libtau:** persist on every append ([f685931](https://github.com/bxrne/tau/commit/f685931dcdfa5d7ede02ceb5ec6e9675496b8fe9))

## [0.2.0](https://github.com/bxrne/tau/compare/libtau-v0.1.0...libtau-v0.2.0) (2026-06-05)


### Features

* **tau, libtau, libdst, dst:** behaviour tree (weighted) driven DST with reference oracle and docs, added tests ([ff1fc79](https://github.com/bxrne/tau/commit/ff1fc795b6aea8f327f641c4f4a6af2b53b8ee06))

## 0.1.0 (2026-06-05)


### Features

* **tau, tauctl, libtau:** the rewrite plus plus ([0acdb87](https://github.com/bxrne/tau/commit/0acdb87b5aa1636fccf4be91565ba24787f0b23f))
