//! User-defined functions: Lua scripts managed via TauQL, run under a
//! capability-gated host API.

mod embed;
mod registry;

pub use registry::{PermissionVerdict, Registry};
