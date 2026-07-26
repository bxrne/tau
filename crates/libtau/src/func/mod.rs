//! User-defined functions: Lua scripts managed via TauQL, run under a
//! capability-gated host API.
//!
//! Modules `embed` (host bridge) and `registry` (function store) are not yet
//! implemented — see the Lua scripting plan. This module exists so the crate
//! compiles while the feature is built incrementally.
