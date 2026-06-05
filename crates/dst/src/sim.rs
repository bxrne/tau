//! Tau [`libdst::DualSimulation`] over direct executor or wire target + isolated oracle.

use libdst::divergence::Divergence;
use libdst::faults::truncate_wal;
use libdst::report::RunResult;
use libdst::sim::{CheckpointAction, DualSimulation};
use libdst::{SequentialOpts, run_sequential};
use libtau::{Executor, wall_clock};
use rand::rngs::StdRng;
use std::cell::RefCell;
use std::fs;
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
        _server: EphemeralServer,
        client: WireClient,
    },
}

pub struct TauSimulation {
    profile: ProfileSpec,
    workspace: ProfileWorkspace,
    target: RefCell<RunTarget>,
    model: RefCell<Oracle>,
    in_transaction: RefCell<bool>,
}

impl TauSimulation {
    pub fn new(profile: ProfileSpec) -> Self {
        wall_clock::set_fixed_now_secs(crate::oracle::DST_NOW_SECS);
        let workspace = ProfileWorkspace::new(profile);
        let model = profile.bootstrap_oracle(&workspace.paths);
        let target = if profile.is_wire() {
            let (_shared, server, client) = profile.spawn_wire_stack(&workspace.paths);
            RunTarget::Wire {
                _server: server,
                client,
            }
        } else {
            RunTarget::Direct(profile.bootstrap_executor(&workspace.paths))
        };
        Self {
            profile,
            workspace,
            target: RefCell::new(target),
            model: RefCell::new(model),
            in_transaction: RefCell::new(false),
        }
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
        RunTarget::Wire {
            _server: server,
            client,
        }
    }

    fn rebuild_model(&self) -> Oracle {
        self.profile.bootstrap_oracle(&self.workspace.paths)
    }

    fn set_target(&self, target: RunTarget) {
        *self.target.borrow_mut() = target;
    }

    fn wipe_disk_dir(&self) {
        if let Some(dir) = self.workspace.paths.disk_dir.as_deref()
            && let Ok(entries) = fs::read_dir(dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "dat") {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    fn replay_dual_log(&self, log: &[Op]) -> CheckpointAction {
        self.wipe_disk_dir();
        let mut model = self.rebuild_model();
        let mut divergences: Vec<Divergence> = Vec::new();
        if self.profile.is_wire() {
            let target = self.rebuild_wire_target();
            self.set_target(target);
            let mut target = self.target.borrow_mut();
            if let RunTarget::Wire { client, .. } = &mut *target {
                replay_log_wire(client, &mut model, log, &mut divergences);
            }
        } else {
            let mut direct = self.rebuild_direct_target();
            for (i, op) in log.iter().enumerate() {
                divergences.extend(apply_dual_executor(i, op, &mut direct, &mut model));
            }
            self.set_target(RunTarget::Direct(direct));
        }
        *self.model.borrow_mut() = model;
        self.finish_replay_state(&mut self.target.borrow_mut());
        if !divergences.is_empty() {
            error!(
                n = divergences.len(),
                profile = %self.profile.name(),
                "disk replay mismatch"
            );
        }
        CheckpointAction::Continue { divergences }
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

    fn wal_truncation_fault(&self, rng: &mut StdRng) {
        let wal_path = self.workspace.paths.wal_path.as_ref().expect("wal path");
        let removed = truncate_wal(wal_path, rng);
        warn!(?removed, profile = %self.profile.name(), "WAL truncated");
        let _ = self.rebuild_direct_target();
        let _ = fs::remove_file(wal_path);
        debug!(profile = %self.profile.name(), "WAL removed after truncation fault");
    }

    fn memory_replay(&self, log: &[Op]) -> CheckpointAction {
        let mut model = self.rebuild_model();
        let mut divergences: Vec<Divergence> = Vec::new();
        if self.profile.is_wire() {
            self.set_target(self.rebuild_wire_target());
            let mut target = self.target.borrow_mut();
            if let RunTarget::Wire { client, .. } = &mut *target {
                replay_log_wire(client, &mut model, log, &mut divergences);
            }
        } else {
            let mut direct = self.rebuild_direct_target();
            for (i, op) in log.iter().enumerate() {
                divergences.extend(apply_dual_executor(i, op, &mut direct, &mut model));
            }
            self.set_target(RunTarget::Direct(direct));
        }
        *self.model.borrow_mut() = model;
        self.finish_replay_state(&mut self.target.borrow_mut());
        if !divergences.is_empty() {
            error!(
                n = divergences.len(),
                profile = %self.profile.name(),
                "memory replay mismatch"
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

fn replay_log_wire(
    client: &mut WireClient,
    model: &mut Oracle,
    log: &[Op],
    divergences: &mut Vec<Divergence>,
) {
    for (i, op) in log.iter().enumerate() {
        divergences.extend(apply_dual(i, op, client, model));
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
        )
    }

    fn apply(&mut self, step: usize, op: &Op) -> Vec<Divergence> {
        let divs = {
            let mut model = self.model.borrow_mut();
            match &mut *self.target.borrow_mut() {
                RunTarget::Direct(ex) => apply_dual_executor(step, op, ex, &mut model),
                RunTarget::Wire { client, .. } => apply_dual(step, op, client, &mut model),
            }
        };
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
            return self.memory_replay(log);
        }

        if self.profile.uses_wal_file() {
            if checkpoint.is_multiple_of(2) {
                self.wal_truncation_fault(rng);
                *self.target.borrow_mut() = RunTarget::Direct(self.rebuild_direct_target());
                *self.model.borrow_mut() = self.rebuild_model();
                self.finish_replay_state(&mut self.target.borrow_mut());
                return CheckpointAction::ResetLog {
                    divergences: vec![],
                };
            }
            return self.wal_dual_replay(log);
        }

        if self.profile.uses_disk_dir() {
            return self.replay_dual_log(log);
        }

        self.memory_replay(log)
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
}
