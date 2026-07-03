//! A capability-based syscall microkernel: direct system calls between isolated
//! subsystems.
//!
//! Services are registered on the [`Kernel`] and accessed only through opaque,
//! typed [`Handle`] capabilities. The kernel provides a [`SyscallCtx`] that enables
//! direct synchronous system calls — no message passing, correlation tracking, or
//! queues. Each syscall completes immediately or returns an error.
//!
//! The core is pure and synchronous: it reads no clock and performs no I/O. All
//! side effects are handled by the [`host`] module. This split keeps the system
//! deterministically replayable.
//!
//! ## Example
//!
//! ```no_run
//! use libtau::kernel::{Kernel, Service, SyscallCtx, SyscallError};
//!
//! struct MyService;
//!
//! impl Service for MyService {
//!     fn boot(&mut self, ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
//!         // Allocate resources, perform syscalls
//!         let handle = ctx.allocate()?;
//!         Ok(())
//!     }
//! }
//! ```

mod engine;
#[cfg(test)]
mod exec_tests;
mod handle;
mod host;
mod policy;
mod types;

pub use engine::{ExternalHandler, Kernel, Service, SyscallCtx, SyscallError, Tick};
pub use handle::{Capability, Handle, RawHandle};
pub use host::{HostBuilder, HostHandle, KernelError, start};
pub use types::{AuthEvent, AuthResult, MetricEvent};
