//! Tau [`libdst::DualSimulation`] over direct executor or wire target + isolated oracle.

use libdst::divergence::Divergence;
use libdst::faults::{corrupt_file, truncate_file};
use libdst::report::RunResult;
use libdst::sim::{CheckpointAction, DualSimulation};
use libdst::{SequentialOpts, run_sequential};
use libtau::{Executor, parse, wall_clock};
use rand::Rng;
use rand::rngs::StdRng;
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, error, warn};

use crate::apply::{apply_dual, apply_dual_executor, sync_transactions};
use crate::btree;
use crate::op::Op;
use crate::oracle::Oracle;
use crate::profile::{CHECKPOINT_EVERY, ProfileSpec, ProfileWorkspace};
use crate::target::{DirectExecutor, Target, WireClient};
use tau::harness::EphemeralServer;

enum RunTarget {
    Direct(Executor),
    Wire {
        server: EphemeralServer,
        client: WireClient,
    },
}

pub struct TauSimulation {
    profile: ProfileSpec,
    workspace: ProfileWorkspace,
    target: RefCell<RunTarget>,
    model: RefCell<Oracle>,
    in_transaction: RefCell<bool>,
    /// Virtual transaction-time counter: advanced once per applied op so each op
    /// (and therefore each append's `written_at`) observes a distinct `now`.
    /// Reset to `0` whenever state is rebuilt from the op log, so live execution
    /// and checkpoint replay stamp identical transaction times.
    tx_tick: RefCell<i64>,
}

impl TauSimulation {
    pub fn new(profile: ProfileSpec) -> Self {
        wall_clock::set_fixed_now_secs(crate::oracle::DST_NOW_SECS);
        let workspace = ProfileWorkspace::new(profile);
        let model = profile.bootstrap_oracle(&workspace.paths);
        let target = if profile.is_wire() {
            let (_shared, server, client) = profile.spawn_wire_stack(&workspace.paths);
            RunTarget::Wire { server, client }
        } else {
            RunTarget::Direct(profile.bootstrap_executor(&workspace.paths))
        };
        Self {
            profile,
            workspace,
            target: RefCell::new(target),
            model: RefCell::new(model),
            in_transaction: RefCell::new(false),
            tx_tick: RefCell::new(0),
        }
    }

    /// Current virtual transaction time (ms) for the next op. Ops are grouped
    /// into clusters that share a generation (see [`crate::oracle::DST_TX_CLUSTER`]).
    fn virtual_now_ms(&self) -> i64 {
        let generation = *self.tx_tick.borrow() / crate::oracle::DST_TX_CLUSTER;
        crate::oracle::DST_TX_BASE_MS + generation * crate::oracle::DST_TX_STEP_MS
    }

    /// Pin the engine + oracle wall clock to the current virtual transaction
    /// time, so an append stamps a deterministic `written_at` and reads observe
    /// a consistent `now`.
    fn stamp_clock(&self) {
        wall_clock::set_fixed_now_ms(self.virtual_now_ms());
    }

    pub fn run(&mut self, n_ops: usize, rng: &mut StdRng) -> RunResult {
        run_sequential(
            SequentialOpts {
                n_ops,
                checkpoint_every: Some(CHECKPOINT_EVERY),
            },
            rng,
            self,
        )
    }

    fn rebuild_direct_target(&self) -> Executor {
        self.profile.bootstrap_executor(&self.workspace.paths)
    }

    fn rebuild_wire_target(&self) -> RunTarget {
        let (_shared, server, client) = self.profile.spawn_wire_stack(&self.workspace.paths);
        RunTarget::Wire { server, client }
    }

    fn rebuild_model(&self) -> Oracle {
        self.profile.bootstrap_oracle(&self.workspace.paths)
    }

    fn set_target(&self, target: RunTarget) {
        *self.target.borrow_mut() = target;
    }

    /// Remove every `.dat` and `.wal` file (plus WAL rotation archives) from
    /// the disk directory. Each disk-backed database now persists across a
    /// `.dat` + `.wal` pair, so both must be wiped together — leaving a stale
    /// `.wal` behind would replay onto a freshly-created `.dat` and diverge
    /// from the freshly-rebuilt oracle.
    fn wipe_disk_dir(&self) {
        if let Some(dir) = self.workspace.paths.disk_dir.as_deref()
            && let Ok(entries) = fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let is_dat = path.extension().is_some_and(|e| e == "dat");
                let is_wal_or_archive = name.contains(".wal");
                if is_dat || is_wal_or_archive {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    /// Wipe `.dat`/`.wal` files and dual-replay the log (disk-backed profiles).
    fn replay_dual_log(&self, log: &[Op]) -> CheckpointAction {
        self.wipe_disk_dir();
        self.dual_replay(log, "disk")
    }

    /// Delete WAL and dual-replay the log (keeps target and oracle aligned).
    fn wal_dual_replay(&self, log: &[Op]) -> CheckpointAction {
        if let Some(wal) = &self.workspace.paths.wal_path {
            let _ = fs::remove_file(wal);
        }
        self.memory_replay(log)
    }

    /// After dual log replay, abort any open transaction on target and oracle.
    fn finish_replay_state(&self, target: &mut RunTarget) {
        let mut model = self.model.borrow_mut();
        sync_transactions_on_target(target, &mut model);
        *self.in_transaction.borrow_mut() = false;
    }

    fn in_transaction_flag(&self) -> bool {
        let model_tx = self.model.borrow().in_transaction();
        let target_tx = match &*self.target.borrow() {
            RunTarget::Direct(ex) => ex.is_in_transaction(),
            RunTarget::Wire { client, .. } => client.is_in_transaction(),
        };
        target_tx || model_tx
    }

    /// Damage the WAL file in place — a short write (`corrupt = false`) or
    /// bit-rot / torn write (`corrupt = true`) — then reopen the WAL-backed
    /// executor. Replaying a damaged WAL must not panic: tau applies the valid
    /// entry prefix and stops at the first bad CRC. The WAL is then removed and
    /// the caller rebuilds from the authoritative op log.
    fn wal_fault(&self, rng: &mut StdRng, corrupt: bool) {
        let wal_path = self.workspace.paths.wal_path.as_ref().expect("wal path");
        let damage = if corrupt {
            corrupt_file(wal_path, rng)
        } else {
            truncate_file(wal_path, rng)
        };
        warn!(
            ?damage,
            kind = if corrupt { "corrupt" } else { "truncate" },
            profile = %self.profile.name(),
            "WAL fault injected",
        );
        // Reopening replays the damaged WAL — the assertion is that it returns
        // (Ok or a clean Err) rather than panicking or hanging.
        self.probe_wal_reopen();
        let _ = fs::remove_file(wal_path);
        debug!(profile = %self.profile.name(), "WAL removed after fault");
    }

    /// Reopen the (possibly damaged) WAL-backed executor. tau replays the valid
    /// entry prefix and either recovers or returns a clean error on the first
    /// bad entry — it must never panic. The result is discarded; unlike
    /// [`Self::rebuild_direct_target`] this tolerates the error path instead of
    /// `expect`-ing a successful open.
    fn probe_wal_reopen(&self) {
        let Some(wal) = self.workspace.paths.wal_path.as_deref() else {
            return;
        };
        match Executor::with_wal_threshold(
            wal,
            self.profile.compact_threshold(),
            self.profile.enc_key(),
        ) {
            Ok(_) => debug!(profile = %self.profile.name(), "wal reopen after fault: recovered"),
            Err(e) => {
                debug!(profile = %self.profile.name(), error = %e, "wal reopen after fault: clean error")
            }
        }
    }

    /// After a WAL fault, rebuild a fresh target + oracle from scratch (the WAL
    /// is gone) and clear the open-transaction flag.
    fn reset_after_wal_fault(&self) {
        *self.target.borrow_mut() = RunTarget::Direct(self.rebuild_direct_target());
        *self.model.borrow_mut() = self.rebuild_model();
        // The op log is reset alongside this rebuild, so restart the virtual
        // write-clock too, keeping subsequent stamps deterministic.
        *self.tx_tick.borrow_mut() = 0;
        self.finish_replay_state(&mut self.target.borrow_mut());
    }

    /// Pick a random `<db>.dat` file from the disk directory, if any exist.
    /// The listing is sorted before the seeded pick so selection is
    /// deterministic for a given seed.
    fn random_dat_file(&self, rng: &mut StdRng) -> Option<PathBuf> {
        let dir = self.workspace.paths.disk_dir.as_deref()?;
        let mut dats: Vec<PathBuf> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "dat"))
            .collect();
        if dats.is_empty() {
            return None;
        }
        dats.sort();
        let idx = rng.gen_range(0..dats.len());
        Some(dats.swap_remove(idx))
    }

    /// Reopen the (possibly damaged) disk backend and force a schema replay of
    /// each `.dat`. tau must recover or return a clean error — never panic. The
    /// result is discarded; the value is the assertion that the call returns.
    fn probe_disk_reopen(&self) {
        let Some(dir) = self.workspace.paths.disk_dir.as_deref() else {
            return;
        };
        match Executor::with_disk_backend(
            dir,
            self.profile.compact_threshold(),
            libtau::storage::DEFAULT_ZSTD_LEVEL,
            self.profile.enc_key(),
            true,
            None,
        ) {
            Ok(mut ex) => {
                // `CREATE DATABASE` replays each `.dat`'s persisted schema,
                // forcing a read of the corrupted bytes.
                for db in ["default", "aux"] {
                    if let Ok((_, stmt)) = parse(&format!("CREATE DATABASE {db}")) {
                        let _ = ex.exec(&stmt);
                    }
                }
                debug!(profile = %self.profile.name(), "disk reopen after fault: recovered");
            }
            Err(e) => {
                debug!(profile = %self.profile.name(), error = %e, "disk reopen after fault: clean error");
            }
        }
    }

    /// Corrupt or truncate a random `.dat` file, then probe that tau survives the
    /// reopen. The caller wipes and rebuilds from the authoritative op log
    /// afterwards, so this only adds the "tau tolerates a bad `.dat`" assertion.
    fn disk_media_fault(&self, rng: &mut StdRng, corrupt: bool) {
        let Some(dat) = self.random_dat_file(rng) else {
            return;
        };
        let damage = if corrupt {
            corrupt_file(&dat, rng)
        } else {
            truncate_file(&dat, rng)
        };
        warn!(
            ?damage,
            file = %dat.display(),
            kind = if corrupt { "corrupt" } else { "truncate" },
            profile = %self.profile.name(),
            "disk .dat fault injected",
        );
        self.probe_disk_reopen();
    }

    /// Network fault: drop the live wire connection and reconnect a fresh client
    /// to the *same* running server. The server keeps its in-memory state (the
    /// executor, including any open transaction, is shared and outlives the
    /// connection), so the oracle and op log are untouched — this exercises the
    /// server's abrupt-disconnect teardown and the client's reconnect + re-auth
    /// path without losing data. Returns `Continue` with no divergences.
    fn network_drop_reconnect(&self) -> CheckpointAction {
        let mut target = self.target.borrow_mut();
        if let RunTarget::Wire { server, client } = &mut *target {
            let addr = format!("{}", server.addr);
            let was_in_tx = client.is_in_transaction();
            // Replacing `client` drops the old socket; the server observes the
            // disconnect and reaps that connection. The new client dials the
            // same address and re-authenticates lazily on its first command.
            let mut fresh = WireClient::connect(&addr, self.profile.transport, self.profile.auth)
                .expect("wire reconnect");
            fresh.set_in_transaction(was_in_tx);
            *client = fresh;
            warn!(
                %addr,
                in_transaction = was_in_tx,
                profile = %self.profile.name(),
                "network connection dropped and reconnected",
            );
        }
        CheckpointAction::Continue {
            divergences: vec![],
        }
    }

    fn memory_replay(&self, log: &[Op]) -> CheckpointAction {
        self.dual_replay(log, "memory")
    }

    /// Rebuild target and model from scratch, replay `log` against both in
    /// lock-step, and install the rebuilt pair.
    fn dual_replay(&self, log: &[Op], label: &str) -> CheckpointAction {
        let mut model = self.rebuild_model();
        let mut divergences: Vec<Divergence> = Vec::new();
        // Replay stamps the virtual write-clock exactly as live execution did:
        // reset the tick to 0 and advance once per op, so every layer's
        // `written_at` — and hence `AS OF` / `HISTORY` — is reproduced bit-for-bit
        // from the authoritative op log.
        *self.tx_tick.borrow_mut() = 0;
        if self.profile.is_wire() {
            self.set_target(self.rebuild_wire_target());
            let mut target = self.target.borrow_mut();
            if let RunTarget::Wire { client, .. } = &mut *target {
                for (i, op) in log.iter().enumerate() {
                    self.stamp_clock();
                    divergences.extend(apply_dual(i, op, client, &mut model));
                    *self.tx_tick.borrow_mut() += 1;
                }
            }
        } else {
            let mut direct = self.rebuild_direct_target();
            for (i, op) in log.iter().enumerate() {
                self.stamp_clock();
                divergences.extend(apply_dual_executor(i, op, &mut direct, &mut model));
                *self.tx_tick.borrow_mut() += 1;
            }
            self.set_target(RunTarget::Direct(direct));
        }
        *self.model.borrow_mut() = model;
        self.finish_replay_state(&mut self.target.borrow_mut());
        if !divergences.is_empty() {
            error!(
                n = divergences.len(),
                profile = %self.profile.name(),
                "{label} replay mismatch"
            );
        }
        CheckpointAction::Continue { divergences }
    }
}

fn sync_transactions_on_target(target: &mut RunTarget, model: &mut Oracle) {
    match target {
        RunTarget::Direct(ex) => {
            sync_transactions(&mut DirectExecutor(ex), model);
        }
        RunTarget::Wire { client, .. } => {
            sync_transactions(client, model);
        }
    }
}

impl DualSimulation for TauSimulation {
    type Op = Op;

    fn pick(&mut self, rng: &mut StdRng) -> Op {
        btree::pick(
            rng,
            &self.model.borrow(),
            self.profile.wal_workload(),
            self.in_transaction_flag(),
            self.virtual_now_ms(),
        )
    }

    fn apply(&mut self, step: usize, op: &Op) -> Vec<Divergence> {
        self.stamp_clock();
        let divs = {
            let mut model = self.model.borrow_mut();
            match &mut *self.target.borrow_mut() {
                RunTarget::Direct(ex) => apply_dual_executor(step, op, ex, &mut model),
                RunTarget::Wire { client, .. } => apply_dual(step, op, client, &mut model),
            }
        };
        *self.tx_tick.borrow_mut() += 1;
        *self.in_transaction.borrow_mut() = self.in_transaction_flag();
        divs
    }

    fn checkpoint(
        &mut self,
        step: usize,
        checkpoint: usize,
        log: &mut Vec<Op>,
        rng: &mut StdRng,
    ) -> CheckpointAction {
        debug!(
            step,
            checkpoint,
            profile = %self.profile.name(),
            log_len = log.len(),
            "checkpoint"
        );

        if self.profile.is_wire() {
            // Alternate two network-layer faults: an abrupt connection drop the
            // server must survive (state intact), and a full server crash that
            // loses the in-memory executor and forces a rebuild + dual-replay.
            if checkpoint.is_multiple_of(2) {
                return self.network_drop_reconnect();
            }
            return self.memory_replay(log);
        }

        if self.profile.uses_wal_file() {
            // Even checkpoints damage the WAL (truncate or corrupt, chosen by the
            // seeded RNG so both kinds fire across the matrix) then rebuild from
            // the op log; odd checkpoints cleanly replay it. Picking the kind
            // from the RNG rather than the checkpoint index means both variants
            // are exercised even in short CI runs with only a couple checkpoints.
            if checkpoint.is_multiple_of(2) {
                let corrupt = rng.gen_bool(0.5);
                self.wal_fault(rng, corrupt);
                self.reset_after_wal_fault();
                CheckpointAction::ResetLog {
                    divergences: vec![],
                }
            } else {
                self.wal_dual_replay(log)
            }
        } else if self.profile.uses_disk_dir() {
            // Even checkpoints damage a random `.dat` (truncate or corrupt) and
            // probe tau's reopen path; every checkpoint then wipes the directory
            // and rebuilds from the authoritative op log, so the damage never
            // perturbs the oracle comparison.
            if checkpoint.is_multiple_of(2) {
                let corrupt = rng.gen_bool(0.5);
                self.disk_media_fault(rng, corrupt);
            }
            self.replay_dual_log(log)
        } else {
            self.memory_replay(log)
        }
    }
}

pub fn run_profile(profile: ProfileSpec, n_ops: usize, rng: &mut StdRng) -> RunResult {
    TauSimulation::new(profile).run(n_ops, rng)
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::profile::spec::{Compaction, Encryption, Storage};
    use crate::profile::{ProfileSpec, Transport};
    use libdst::{SequentialOpts, run_sequential};
    use rand::SeedableRng;

    #[test]
    fn disk_profile_seed_1_clean() {
        let mut rng = StdRng::seed_from_u64(1);
        let disk = ProfileSpec {
            storage: Storage::Disk,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let r = run_profile(disk, disk.ci_ops(), &mut rng);
        assert_eq!(r.errors, 0, "disk profile had {} errors", r.errors);
    }

    #[test]
    fn wal_default_seed_1_clean() {
        let mut rng = StdRng::seed_from_u64(1);
        let p = ProfileSpec {
            storage: Storage::Wal,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let r = run_profile(p, p.ci_ops(), &mut rng);
        assert_eq!(r.errors, 0, "wal default had {} errors", r.errors);
    }

    #[test]
    fn smoke_profiles_reseed_clean() {
        let seed = 1u64;
        for profile in ProfileSpec::engine_matrix(crate::profile::SuiteTier::Smoke) {
            let mut rng = StdRng::seed_from_u64(seed);
            let r = run_profile(profile, profile.ci_ops(), &mut rng);
            assert_eq!(
                r.errors,
                0,
                "profile {} had {} errors",
                profile.name(),
                r.errors
            );
        }
    }

    #[test]
    fn standard_all_profiles_ci_ops_seed_1() {
        let seed = 1u64;
        for profile in ProfileSpec::all_for_tier(crate::profile::SuiteTier::Standard) {
            let mut rng = StdRng::seed_from_u64(seed);
            let r = run_profile(profile, profile.ci_ops(), &mut rng);
            assert_eq!(
                r.errors,
                0,
                "profile {} had {} errors",
                profile.name(),
                r.errors
            );
        }
    }

    #[test]
    fn nightly_all_profiles_ci_ops_seed_1() {
        let seed = 1u64;
        for profile in ProfileSpec::all_for_tier(crate::profile::SuiteTier::Nightly) {
            let mut rng = StdRng::seed_from_u64(seed);
            let r = run_profile(profile, profile.ci_ops(), &mut rng);
            assert_eq!(
                r.errors,
                0,
                "profile {} had {} errors",
                profile.name(),
                r.errors
            );
        }
    }

    #[test]
    fn memory_stress_seed_1_clean() {
        let mut rng = StdRng::seed_from_u64(1);
        let p = ProfileSpec {
            storage: Storage::Memory,
            compaction: Compaction::Stress,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let r = run_profile(p, p.ci_ops(), &mut rng);
        assert_eq!(r.errors, 0, "memory stress had {} errors", r.errors);
    }

    #[test]
    fn smoke_profiles_multiple_seeds() {
        for seed in [1u64, 2, 3, 7, 42, 99] {
            for profile in ProfileSpec::engine_matrix(crate::profile::SuiteTier::Smoke) {
                let mut rng = StdRng::seed_from_u64(seed);
                let r = run_profile(profile, 300, &mut rng);
                assert_eq!(
                    r.errors,
                    0,
                    "seed {seed} profile {} had {} errors",
                    profile.name(),
                    r.errors
                );
            }
        }
    }

    #[test]
    fn memory_default_seed_2_ci_ops() {
        let mut rng = StdRng::seed_from_u64(2);
        let p = ProfileSpec {
            storage: Storage::Memory,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let r = run_profile(p, p.ci_ops(), &mut rng);
        assert_eq!(r.errors, 0, "memory default seed 2 had {} errors", r.errors);
    }

    #[test]
    fn memory_default_seed_1_clean() {
        let mut rng = StdRng::seed_from_u64(1);
        let p = ProfileSpec {
            storage: Storage::Memory,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let r = run_profile(p, p.ci_ops(), &mut rng);
        assert_eq!(r.errors, 0, "memory default had {} errors", r.errors);
    }

    #[test]
    fn disk_profile_seed_1_no_checkpoints() {
        let mut rng = StdRng::seed_from_u64(1);
        let disk = ProfileSpec {
            storage: Storage::Disk,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let mut sim = TauSimulation::new(disk);
        let r = run_sequential(
            SequentialOpts {
                n_ops: disk.ci_ops(),
                checkpoint_every: None,
            },
            &mut rng,
            &mut sim,
        );
        assert_eq!(r.errors, 0, "disk no-checkpoint had {} errors", r.errors);
    }

    /// Faithful disk restart (no wipe, no op-log replay to target): after writes
    /// that each went through the per-database WAL (fsynced by default),
    /// re-opening the executor over the existing .dat + .wal files must see the
    /// same data and schema that the oracle has. This exercises the disk
    /// backend's append_schema + append + WAL-replay paths + executor's
    /// CREATE DATABASE schema replay for a real process restart.
    #[test]
    fn pbt_disk_persists_data_and_schema_across_reopen() {
        use crate::apply::apply_dual_executor;
        use crate::harness;
        use crate::op::Op;
        use crate::oracle::DeriveSpec;
        use crate::target::DirectExecutor;

        let disk = ProfileSpec {
            storage: Storage::Disk,
            compaction: Compaction::Default,
            encryption: Encryption::Plain,
            transport: Transport::Direct,
            auth: crate::profile::Auth::Off,
        };
        let workspace = crate::profile::ProfileWorkspace::new(disk);
        let paths = &workspace.paths;

        let mut target = disk.bootstrap_executor(paths);
        let mut model = disk.bootstrap_oracle(paths);

        // A small, varied sequence exercising DDL (extra lenses, derive, drop) + DML (appends)
        // and probes. All go through dual-apply so target and model stay in sync.
        // Use lenses from the DST bootstrap set ("a","b" are the int ones) plus the new "p".
        // (No TTL here; TTL persistence is covered by dedicated unit regression tests using
        // an ancient datum + small TTL window relative to the fixed DST wall clock.)
        let ops: Vec<Op> = vec![
            Op::CreateLens {
                name: "p".into(),
                ty: "int",
            },
            Op::Append {
                lens: "p".into(),
                data: crate::op::Payload::Int(vec![(0, 100, 1), (100, 200, 2)]),
            },
            Op::Derive {
                name: "p2".into(),
                spec: DeriveSpec {
                    a: "p".into(),
                    b: "a".into(),
                },
            },
            Op::Append {
                lens: "a".into(),
                data: crate::op::Payload::Int(vec![(10, 50, 99)]),
            },
            Op::DropLens { name: "p2".into() }, // dropped derived should stay dropped after restart
        ];

        for (i, op) in ops.iter().enumerate() {
            let divs = apply_dual_executor(i, op, &mut target, &mut model);
            assert!(divs.is_empty(), "pre-restart divergence at {i}: {divs:?}");
            // Right after the append that populates p, the data must be visible on target (before later DDL).
            if i == 1 {
                let imm = harness::exec(&mut target, "AT LENS p 50");
                let v = match imm {
                    libtau::Output::Value(v) => v,
                    other => panic!("immediate post-append-p AT unexpected: {other:?}"),
                };
                assert_eq!(
                    v,
                    Some(libtau::Value::Int(1)),
                    "target cannot see data immediately after its append to p (step {i})"
                );
            }
            // After each later DDL/DML, re-probe p to see if a subsequent op made prior data invisible.
            if i >= 2 {
                let probe = harness::exec(&mut target, "AT LENS p 50");
                let v = match probe {
                    libtau::Output::Value(v) => v,
                    other => panic!("AT p 50 after op {i} unexpected variant: {other:?}"),
                };
                assert_eq!(
                    v,
                    Some(libtau::Value::Int(1)),
                    "p data disappeared from target after op {i} (derive/append/drop side effect?)"
                );
            }
        }

        // Sanity: the original target (pre-drop) must itself see the data we just appended.
        // If this fails, the problem is in apply/append visibility, not restart persistence.
        let pre_p = harness::exec(&mut target, "AT LENS p 50");
        let pre_p_val = match pre_p {
            libtau::Output::Value(v) => v,
            other => panic!("pre-restart AT p 50 unexpected: {other:?}"),
        };
        assert_eq!(
            pre_p_val,
            Some(libtau::Value::Int(1)),
            "original target cannot see its own append to p"
        );

        // Drop the target (ensuring last flush completed) and reopen from the on-disk files.
        drop(target);
        let mut restarted = disk.reopen_disk_executor(paths);
        // Ensure tx flags are consistent (no tx in this sequence, but keep the helper honest).
        crate::apply::sync_transactions(&mut DirectExecutor(&mut restarted), &mut model);

        // Post-restart, the reopened target must match the oracle for data and names.
        // (Schema for p, the appends to p and i0, the TTL on p, and absence of p2 were all
        // persisted via append_schema/append + flush on each write.)
        for (lens, t, expected) in [
            ("p", 50, Some(libtau::Value::Int(1))),
            ("p", 150, Some(libtau::Value::Int(2))),
            ("a", 30, Some(libtau::Value::Int(99))),
            ("a", 1000, None),
        ] {
            let out = harness::exec(&mut restarted, &format!("AT LENS {lens} {t}"));
            let got = match out {
                libtau::Output::Value(v) => v,
                other => panic!("expected Value for AT, got {other:?}"),
            };
            assert_eq!(got, expected, "AT LENS {lens} {t} mismatch after reopen");
        }

        // p2 was dropped before restart; it must still be unknown (DROP persisted).
        let (_, stmt) = libtau::parse("AT LENS p2 0").expect("parse p2 at");
        let direct_res = restarted.exec(&stmt);
        assert!(
            direct_res.is_err(),
            "p2 should remain dropped after reopen: {direct_res:?}"
        );

        // SHOW LENSES should not include the dropped p2 (but will include the base set + p).
        if let libtau::Output::Names(names) = harness::exec(&mut restarted, "SHOW LENSES") {
            assert!(
                !names.iter().any(|n| n == "p2"),
                "p2 must not appear after restart"
            );
            assert!(names.iter().any(|n| n == "p"), "p must survive restart");
        } else {
            panic!("SHOW LENSES did not return Names");
        }
    }
}
