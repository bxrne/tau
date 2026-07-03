//! Metrics service for the syscall kernel.
//!
//! This module provides the Service implementation and syscall routing
//! for metrics collection.

use std::sync::Arc;

use crate::MetricEvent;
use crate::kernel::{Service, SyscallCtx, SyscallError};

mod core;

pub use core::{Metrics, OP_COUNT, OP_LABELS, Op};

pub struct MetricsService {
    metrics: Arc<Metrics>,
}

impl MetricsService {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self { metrics }
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }
}

impl Service for MetricsService {
    fn boot(&mut self, _ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
        // Metrics don't need initialization - just acknowledge we're ready
        Ok(())
    }
}

impl MetricsService {
    /// Handle a metric event from the syscall interface.
    pub fn handle_metric_event(&mut self, event: MetricEvent) -> Result<(), SyscallError> {
        match event {
            MetricEvent::Op { op, ns } => {
                self.metrics.record_op(op, ns);
            }
            MetricEvent::DbOp { db, op } => {
                self.metrics.record_db_op(&db, op);
            }
            MetricEvent::Compaction => {
                self.metrics.record_compaction();
            }
            MetricEvent::WalWrite { ns } => {
                self.metrics.record_wal_write(ns);
            }
            MetricEvent::SetActiveConnections { n } => {
                self.metrics.set_active_connections(n);
            }
            MetricEvent::ConnectionAccepted => {
                self.metrics.connections.inc();
            }
            MetricEvent::ConnectionRejected => {
                self.metrics.record_rejected_connection();
            }
            MetricEvent::AuthAttempt => {
                self.metrics.record_auth_attempt();
            }
            MetricEvent::AuthFailure => {
                self.metrics.record_auth_failure();
            }
            MetricEvent::Error => {
                self.metrics.record_error();
            }
        }
        Ok(())
    }

    /// Get metrics as prometheus text format.
    pub fn get_metrics(&self) -> Result<String, SyscallError> {
        Ok(self.metrics.prometheus_text())
    }
}
