//! Profile matrix + bootstrap for target and isolated oracle.

#[cfg(test)]
mod fixture;
pub mod spec;

#[cfg(test)]
pub use fixture::MEMORY_MULTI;
pub use spec::{Auth, ProfileSpec, Storage, SuiteTier, Transport};
#[cfg(test)]
pub use spec::{Compaction, Encryption};

use std::path::{Path, PathBuf};

use std::collections::HashMap;
use std::sync::Arc;

use libtau::services::auth::Perm;
use libtau::{Kernel, User, parse};
use tau::harness::{EphemeralServer, HarnessOpts};
use tempfile::TempDir;

use crate::harness::exec;
use crate::op::{BOOL, FL, INT, Lens, SV};
use crate::oracle::Oracle;
use crate::target::{DST_AUTH_PASS, DST_AUTH_USER, WireClient};

/// Checkpoint interval for fault injection (all sequential profiles).
pub const CHECKPOINT_EVERY: usize = 200;

/// Per-run filesystem paths (WAL file or disk root).
pub struct ProfilePaths {
    pub wal_path: Option<PathBuf>,
    pub disk_dir: Option<PathBuf>,
}

pub struct ProfileWorkspace {
    _wal_dir: Option<TempDir>,
    _disk_dir: Option<TempDir>,
    pub paths: ProfilePaths,
}

impl ProfileWorkspace {
    pub fn new(profile: ProfileSpec) -> Self {
        let wal_dir = profile
            .uses_wal_file()
            .then(tempfile::tempdir)
            .transpose()
            .expect("tempdir");
        let disk_dir = profile
            .uses_disk_dir()
            .then(tempfile::tempdir)
            .transpose()
            .expect("tempdir");
        let paths = ProfilePaths {
            wal_path: wal_dir.as_ref().map(|d| d.path().join("sim.wal")),
            disk_dir: disk_dir.as_ref().map(|d| d.path().to_path_buf()),
        };
        Self {
            _wal_dir: wal_dir,
            _disk_dir: disk_dir,
            paths,
        }
    }
}

impl ProfileSpec {
    pub fn bootstrap_kernel(&self, paths: &ProfilePaths) -> Kernel {
        let kernel = match self.storage {
            Storage::Memory => bootstrap_memory(self.compact_threshold(), self.multi_db()),
            Storage::Wal => {
                let p = paths.wal_path.as_ref().expect("wal path");
                bootstrap_wal(p, self.compact_threshold(), self.enc_key())
            }
            Storage::Disk => {
                let d = paths.disk_dir.as_ref().expect("disk dir");
                bootstrap_disk(d, self.compact_threshold(), self.enc_key())
            }
        };
        // Pin this kernel's own virtual clock to the DST epoch. Per-kernel:
        // simulations never share clock state, so they can run in parallel.
        kernel
            .clock()
            .set_fixed_now_ms(crate::oracle::DST_TX_BASE_MS);
        kernel
    }

    /// Fresh isolated reference model — never derived from kernel state.
    pub fn bootstrap_oracle(self, paths: &ProfilePaths) -> Oracle {
        bootstrap_oracle_model(self, paths)
    }

    /// Reopen the disk-backed kernel from its existing manifest/run files
    /// without re-issuing DDL — models a process restart for the persistence
    /// checks.
    #[cfg(test)]
    pub fn reopen_disk_kernel(&self, paths: &ProfilePaths) -> Kernel {
        let d = paths.disk_dir.as_ref().expect("disk dir");
        let kernel = reopen_disk(d, self.compact_threshold(), self.enc_key());
        kernel
            .clock()
            .set_fixed_now_ms(crate::oracle::DST_TX_BASE_MS);
        kernel
    }

    /// Shared kernel + ephemeral tau server for wire transport profiles.
    pub fn spawn_wire_stack(
        &self,
        paths: &ProfilePaths,
    ) -> (Arc<Kernel>, EphemeralServer, WireClient) {
        assert!(self.is_wire(), "spawn_wire_stack requires wire transport");
        let mut ex = self.bootstrap_kernel(paths);
        if matches!(self.auth, Auth::On) {
            install_dst_auth(&mut ex);
        }
        let shared = Arc::new(ex);
        let server = EphemeralServer::spawn(
            Arc::clone(&shared),
            HarnessOpts {
                tls: matches!(self.transport, Transport::WireTls),
                auth: matches!(self.auth, Auth::On),
                connection_limit: 64,
            },
        )
        .expect("ephemeral tau server");
        let addr = format!("{}", server.addr);
        let client =
            WireClient::connect(&addr, self.transport, self.auth).expect("wire client connect");
        (shared, server, client)
    }
}

fn install_dst_auth(ex: &mut Kernel) {
    let mut grants = HashMap::new();
    grants.insert("*".to_string(), Perm::ALL);
    ex.auth()
        .users()
        .lock()
        .expect("user store lock poisoned")
        .add(User::new(DST_AUTH_USER, DST_AUTH_PASS, grants))
        .expect("dst auth user");
}

fn bootstrap_memory(threshold: usize, multi_db: bool) -> Kernel {
    let mut ex = Kernel::with_threshold(threshold);
    exec(&mut ex, "CREATE DATABASE default");
    for lens in INT {
        exec(&mut ex, &format!("CREATE LENS {lens} int"));
    }
    exec(&mut ex, &format!("CREATE LENS {FL} float"));
    exec(&mut ex, &format!("CREATE LENS {BOOL} bool"));
    exec(&mut ex, &format!("CREATE LENS {SV} str"));
    if multi_db {
        exec(&mut ex, "CREATE DATABASE aux");
        exec(&mut ex, "USE DATABASE aux");
        exec(&mut ex, &format!("CREATE LENS {} int", Lens::Aux.as_str()));
        exec(&mut ex, "USE DATABASE default");
    }
    ex
}

fn bootstrap_wal(wal_path: &Path, threshold: usize, enc_key: Option<[u8; 32]>) -> Kernel {
    let ex = Kernel::with_wal_threshold(wal_path, threshold, enc_key).expect("WAL open");
    for lens in INT {
        let (_, stmt) = parse(&format!("CREATE LENS {lens} int")).unwrap();
        let _ = ex.exec(&stmt);
    }
    for (name, ty) in [(FL, "float"), (BOOL, "bool"), (SV, "str")] {
        let (_, stmt) = parse(&format!("CREATE LENS {name} {ty}")).unwrap();
        let _ = ex.exec(&stmt);
    }
    ex
}

fn bootstrap_disk(dir: &Path, threshold: usize, enc_key: Option<[u8; 32]>) -> Kernel {
    let mut ex = Kernel::with_disk_backend(
        dir,
        threshold,
        libtau::DEFAULT_ZSTD_LEVEL,
        enc_key,
        true,
        None,
    )
    .expect("disk backend open");
    exec(&mut ex, "CREATE DATABASE default");
    for lens in INT {
        exec(&mut ex, &format!("CREATE LENS {lens} int"));
    }
    exec(&mut ex, &format!("CREATE LENS {FL} float"));
    exec(&mut ex, &format!("CREATE LENS {BOOL} bool"));
    exec(&mut ex, &format!("CREATE LENS {SV} str"));
    exec(&mut ex, "CREATE DATABASE aux");
    exec(&mut ex, "USE DATABASE aux");
    exec(&mut ex, &format!("CREATE LENS {} int", Lens::Aux.as_str()));
    exec(&mut ex, "USE DATABASE default");
    ex
}

/// Reopen an existing disk-backed kernel **without** re-issuing schema DDL —
/// `CREATE DATABASE` replays each database's persisted manifest schema, so
/// lenses and policies come back automatically. Models a real process
/// restart (used by the disk-persistence DST coverage).
#[cfg(test)]
pub fn reopen_disk(dir: &Path, threshold: usize, enc_key: Option<[u8; 32]>) -> Kernel {
    let mut ex = Kernel::with_disk_backend(
        dir,
        threshold,
        libtau::DEFAULT_ZSTD_LEVEL,
        enc_key,
        true,
        None,
    )
    .expect("disk backend reopen");
    exec(&mut ex, "CREATE DATABASE default");
    exec(&mut ex, "CREATE DATABASE aux");
    exec(&mut ex, "USE DATABASE default");
    ex
}

fn bootstrap_oracle_model(profile: ProfileSpec, _paths: &ProfilePaths) -> Oracle {
    let mut o = Oracle::with_threshold(profile.compact_threshold());
    for lens in INT {
        o.create_lens(lens);
    }
    o.create_lens(FL);
    o.create_lens(BOOL);
    o.create_lens(SV);
    if profile.multi_db() {
        o.create_db("aux");
        o.use_db("aux");
        o.create_lens(Lens::Aux.as_str());
        o.use_db("default");
    }
    o
}
