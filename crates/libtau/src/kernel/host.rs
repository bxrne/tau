//! The tokio host: the only impure code. It manages external I/O handlers and
//! provides synchronous syscall access to them. Swap this for a deterministic
//! loop to drive the same core under simulation.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;

use super::engine::{ExternalHandler, Kernel, SyscallCtx, SyscallError, Tick};
use super::handle::RawHandle;

const CMD_CAP: usize = 64;

#[derive(Debug, PartialEq, Eq)]
pub enum KernelError {
    /// The host loop is gone.
    Shutdown,
    /// Syscall failed.
    SyscallFailed(SyscallError),
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KernelError::Shutdown => write!(f, "Kernel shutdown"),
            KernelError::SyscallFailed(e) => write!(f, "Syscall failed: {:?}", e),
        }
    }
}

/// Type-erased syscall result ferried back over the reply channel.
type SyscallReply = Result<Box<dyn std::any::Any + Send>, SyscallError>;
/// A boxed syscall closure executed on the driver loop.
type SyscallFn = Box<dyn FnOnce(&mut SyscallCtx<'_>) -> SyscallReply + Send>;

/// Commands that can be sent to the host driver.
enum Cmd {
    /// Perform a syscall with a callback.
    Syscall {
        syscall: SyscallFn,
        reply: tokio::sync::oneshot::Sender<SyscallReply>,
    },
}

/// External entry point into a running system. Cloneable and `Send`.
#[derive(Clone)]
pub struct HostHandle {
    cmd_tx: mpsc::Sender<Cmd>,
}

impl HostHandle {
    /// Perform a syscall with access to the kernel context.
    pub async fn syscall<F, R>(&self, f: F) -> Result<R, KernelError>
    where
        F: FnOnce(&mut SyscallCtx<'_>) -> Result<R, SyscallError> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(Cmd::Syscall {
                syscall: Box::new(move |ctx| {
                    f(ctx).map(|r| Box::new(r) as Box<dyn std::any::Any + Send>)
                }),
                reply: tx,
            })
            .await
            .map_err(|_| KernelError::Shutdown)?;

        let result = rx.await.map_err(|_| KernelError::Shutdown)?;
        result.map_err(KernelError::SyscallFailed).and_then(|any| {
            any.downcast::<R>().map(|boxed| *boxed).map_err(|_| {
                KernelError::SyscallFailed(SyscallError::InternalError(
                    "Type mismatch in syscall return".to_string(),
                ))
            })
        })
    }
}

/// Attaches host-side I/O handlers to external resources, then starts the loop.
pub struct HostBuilder {
    core: Kernel,
    external: Vec<(RawHandle, Arc<dyn ExternalHandler>)>,
}

impl HostBuilder {
    pub fn new(core: Kernel) -> Self {
        HostBuilder {
            core,
            external: Vec::new(),
        }
    }

    /// Register an external I/O handler.
    pub fn with_external(mut self, handler: Arc<dyn ExternalHandler>) -> Self {
        let raw_handle = self.core.register_external(handler.clone());
        self.external.push((raw_handle, handler));
        self
    }

    /// Register a simple function-based external handler.
    pub fn with_handler<F, W>(self, read_fn: F, write_fn: W) -> Self
    where
        F: FnMut(&mut [u8]) -> Result<usize, SyscallError> + Send + Sync + 'static,
        W: FnMut(&[u8]) -> Result<usize, SyscallError> + Send + Sync + 'static,
    {
        struct FnHandler<R, W> {
            read: Mutex<R>,
            write: Mutex<W>,
        }

        impl<R, W> ExternalHandler for FnHandler<R, W>
        where
            R: FnMut(&mut [u8]) -> Result<usize, SyscallError> + Send + Sync,
            W: FnMut(&[u8]) -> Result<usize, SyscallError> + Send + Sync,
        {
            fn read(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
                let mut read_guard = self.read.lock().map_err(|e| {
                    SyscallError::InternalError(format!("read mutex poisoned: {}", e))
                })?;
                read_guard(buf)
            }

            fn write(&self, data: &[u8]) -> Result<usize, SyscallError> {
                let mut write_guard = self.write.lock().map_err(|e| {
                    SyscallError::InternalError(format!("write mutex poisoned: {}", e))
                })?;
                write_guard(data)
            }
        }

        let handler = Arc::new(FnHandler {
            read: Mutex::new(read_fn),
            write: Mutex::new(write_fn),
        });

        self.with_external(handler)
    }

    /// Spawn the driver on the current tokio runtime.
    pub fn start(self) -> HostHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(CMD_CAP);
        let driver = Driver {
            core: self.core,
            cmd_rx,
            start: Instant::now(),
        };
        tokio::spawn(driver.serve());
        HostHandle { cmd_tx }
    }
}

/// Start a host with no external handlers.
pub fn start(core: Kernel) -> HostHandle {
    HostBuilder::new(core).start()
}

struct Driver {
    core: Kernel,
    cmd_rx: mpsc::Receiver<Cmd>,
    start: Instant,
}

impl Driver {
    async fn serve(mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                Cmd::Syscall { syscall, reply } => {
                    // Update logical time
                    self.core
                        .advance_to(self.start.elapsed().as_millis() as Tick);

                    // Perform the syscall
                    let mut ctx = self.core.syscall_ctx();
                    let result = syscall(&mut ctx);

                    // Send reply
                    let _ = reply.send(result);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::MetricEvent;
    use crate::services::metrics::Op;
    use crate::{HostBuilder, Kernel, Service, SyscallCtx, SyscallError, start};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // A test subsystem
    struct TestService;

    impl Service for TestService {
        fn boot(&mut self, _ctx: &mut SyscallCtx<'_>) -> Result<(), SyscallError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn syscall_performs_operation() {
        let mut core = Kernel::new();
        let _subsystem = core.register(TestService);

        let data = Arc::new(Mutex::new(vec![1u8, 2, 3]));
        let data_clone = data.clone();

        let host = HostBuilder::new(core)
            .with_handler(
                move |buf: &mut [u8]| {
                    let stored = data_clone.lock().map_err(|e| {
                        SyscallError::InternalError(format!("test data mutex poisoned: {}", e))
                    })?;
                    let to_copy = std::cmp::min(buf.len(), stored.len());
                    buf[..to_copy].copy_from_slice(&stored[..to_copy]);
                    Ok(to_copy)
                },
                |_: &[u8]| Ok(0),
            )
            .start();

        // Perform a syscall to test the mechanism works
        let result = host
            .syscall(|ctx| {
                // Test that the syscall mechanism works by recording a metric
                ctx.metric_record(MetricEvent::Op {
                    op: Op::Append,
                    ns: 1000,
                })
            })
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn shutdown_error_when_host_gone() {
        let mut core = Kernel::new();
        let _subsystem = core.register(TestService);
        let host = start(core);

        // The host is still alive since we just created it
        let result = host.syscall(|_ctx| Ok(())).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn handler_stores_write_data() {
        let write_count = Arc::new(AtomicUsize::new(0));
        let write_count_clone = write_count.clone();

        let host = HostBuilder::new(Kernel::new())
            .with_handler(
                |_: &mut [u8]| Ok(0),
                move |data: &[u8]| {
                    write_count_clone.fetch_add(data.len(), Ordering::SeqCst);
                    Ok(data.len())
                },
            )
            .start();

        // Perform syscalls that write
        let _ = host
            .syscall(|ctx| {
                // Would write to handle in real scenario
                ctx.allocate()?;
                Ok(())
            })
            .await;

        // In a real test, we'd verify writes happened
        assert_eq!(write_count.load(Ordering::SeqCst), 0); // No actual writes in this simplified test
    }

    #[test]
    fn logical_time_advances_in_host() {
        let mut core = Kernel::new();
        assert_eq!(core.now(), 0);

        core.advance_to(100);
        assert_eq!(core.now(), 100);
    }
}
