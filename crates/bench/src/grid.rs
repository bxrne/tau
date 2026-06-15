//! The benchmark config grid.
//!
//! Mirrors the dimensions of `tau::config::Config` (backend, wal, tls, auth,
//! encryption at rest, compression level, compaction threshold) as a small
//! set of named cells, grouped into presets. Each cell is turned into a
//! `libtau::Executor` (for the engine layer) plus `HarnessOpts` (for the wire
//! layer's `EphemeralServer`).
//!
//! This is intentionally a bench-local type, not `tau::Config` itself: the
//! `Config` struct's fields are private to the `tau` crate. A shared
//! `ConfigMatrix` usable by both `bench` and `dst` is tracked as a follow-up
//! (see `crates/bench/README.md`).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use libtau::{Executor, Perm, User, UserStore};
use tau::harness::HarnessOpts;

/// A fixed, non-secret key used only to exercise the AES-256-GCM
/// encryption-at-rest code path during benchmarks.
const BENCH_ENC_KEY: [u8; 32] = [0x42; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Memory,
    Disk,
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigCell {
    pub name: &'static str,
    pub backend: Backend,
    pub wal: bool,
    pub wal_fsync_each: bool,
    pub tls: bool,
    pub auth: bool,
    pub encryption: bool,
    pub compression_level: i32,
    pub compact_threshold: usize,
}

impl Default for ConfigCell {
    fn default() -> Self {
        Self {
            name: "default",
            backend: Backend::Memory,
            wal: false,
            wal_fsync_each: true,
            tls: false,
            auth: false,
            encryption: false,
            compression_level: libtau::storage::DEFAULT_ZSTD_LEVEL,
            compact_threshold: libtau::storage::COMPACT_THRESHOLD,
        }
    }
}

impl ConfigCell {
    fn named(name: &'static str) -> Self {
        Self {
            name,
            ..Self::default()
        }
    }

    fn enc_key(&self) -> Option<[u8; 32]> {
        self.encryption.then_some(BENCH_ENC_KEY)
    }

    /// Build a fresh `Executor` for this cell. `dir` is used by disk-backed
    /// and WAL-backed cells for their on-disk files; callers should pass a
    /// per-benchmark temporary directory.
    pub fn build_executor(&self, dir: &Path) -> io::Result<Executor> {
        let mut executor = match self.backend {
            Backend::Disk => Executor::with_disk_backend(
                dir,
                self.compact_threshold,
                self.compression_level,
                self.enc_key(),
                self.wal_fsync_each,
                None,
            )?,
            Backend::Memory if self.wal => Executor::with_wal_threshold(
                dir.join("bench.wal"),
                self.compact_threshold,
                self.enc_key(),
            )?,
            Backend::Memory => Executor::with_threshold(self.compact_threshold),
        };

        if self.auth {
            let mut store = UserStore::new();
            let mut grants: HashMap<String, Perm> = HashMap::default();
            grants.insert("*".to_string(), Perm::ALL);
            store
                .add(User::new("admin", "admin", grants))
                .expect("add bench admin user");
            executor.set_users(store);
        }

        Ok(executor)
    }

    /// `HarnessOpts` for `EphemeralServer::spawn`, matching this cell's
    /// `tls`/`auth` settings.
    pub fn harness_opts(&self) -> HarnessOpts {
        HarnessOpts {
            tls: self.tls,
            auth: self.auth,
            connection_limit: 64,
        }
    }
}

/// A handful of cells covering the storage/security axes most relevant for
/// quick, repeatable runs (CI smoke, capped Docker numbers).
pub fn quick() -> Vec<ConfigCell> {
    vec![
        ConfigCell::named("memory"),
        ConfigCell {
            wal: true,
            ..ConfigCell::named("memory_wal")
        },
        ConfigCell {
            backend: Backend::Disk,
            ..ConfigCell::named("disk")
        },
        ConfigCell {
            tls: true,
            ..ConfigCell::named("tls")
        },
    ]
}

/// Security-focused cells: tls and auth and encryption combinations at a
/// fixed memory backend.
pub fn security() -> Vec<ConfigCell> {
    vec![
        ConfigCell::named("plain"),
        ConfigCell {
            tls: true,
            ..ConfigCell::named("tls")
        },
        ConfigCell {
            auth: true,
            ..ConfigCell::named("auth")
        },
        ConfigCell {
            tls: true,
            auth: true,
            ..ConfigCell::named("tls_auth")
        },
        ConfigCell {
            wal: true,
            encryption: true,
            ..ConfigCell::named("wal_encrypted")
        },
        ConfigCell {
            backend: Backend::Disk,
            encryption: true,
            ..ConfigCell::named("disk_encrypted")
        },
        ConfigCell {
            backend: Backend::Disk,
            tls: true,
            auth: true,
            encryption: true,
            ..ConfigCell::named("disk_tls_auth_encrypted")
        },
    ]
}

/// Storage-focused cells: backend, wal, compression, with security off.
pub fn storage() -> Vec<ConfigCell> {
    vec![
        ConfigCell::named("memory"),
        ConfigCell {
            wal: true,
            wal_fsync_each: true,
            ..ConfigCell::named("memory_wal_fsync")
        },
        ConfigCell {
            wal: true,
            wal_fsync_each: false,
            ..ConfigCell::named("memory_wal_grouped")
        },
        ConfigCell {
            backend: Backend::Disk,
            compression_level: 1,
            ..ConfigCell::named("disk_zstd1")
        },
        ConfigCell {
            backend: Backend::Disk,
            compression_level: 19,
            ..ConfigCell::named("disk_zstd19")
        },
        ConfigCell {
            compact_threshold: 4,
            ..ConfigCell::named("compact_low")
        },
        ConfigCell {
            compact_threshold: 64,
            ..ConfigCell::named("compact_high")
        },
    ]
}

/// The full cartesian-ish product. Opt-in only: large and slow.
pub fn full() -> Vec<ConfigCell> {
    let mut cells = Vec::new();
    for &backend in &[Backend::Memory, Backend::Disk] {
        for &wal in &[false, true] {
            for &tls in &[false, true] {
                for &auth in &[false, true] {
                    for &encryption in &[false, true] {
                        cells.push(ConfigCell {
                            name: "full",
                            backend,
                            wal,
                            wal_fsync_each: true,
                            tls,
                            auth,
                            encryption,
                            compression_level: libtau::storage::DEFAULT_ZSTD_LEVEL,
                            compact_threshold: libtau::storage::COMPACT_THRESHOLD,
                        });
                    }
                }
            }
        }
    }
    cells
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Preset {
    Quick,
    Security,
    Storage,
    Full,
}

impl Preset {
    pub fn cells(&self) -> Vec<ConfigCell> {
        match self {
            Preset::Quick => quick(),
            Preset::Security => security(),
            Preset::Storage => storage(),
            Preset::Full => full(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_nonempty_and_named() {
        for preset in [Preset::Quick, Preset::Security, Preset::Storage] {
            let cells = preset.cells();
            assert!(!cells.is_empty());
            for cell in &cells {
                assert!(!cell.name.is_empty());
            }
        }
    }

    #[test]
    fn full_preset_covers_every_axis_combination() {
        // 2 backends * 2 wal * 2 tls * 2 auth * 2 encryption
        assert_eq!(full().len(), 32);
    }

    #[test]
    fn build_executor_works_for_every_quick_cell() {
        for cell in quick() {
            let dir = tempfile::tempdir().expect("tempdir");
            cell.build_executor(dir.path()).expect("build executor");
        }
    }
}
