//! The Lua host bridge: installs a `tau.*` table into a scoped `Lua` state,
//! binding capability-gated closures that re-enter the kernel.
//!
//! The [`HostData`] cell must be created by the caller *before* `lua.scope()`
//! and passed in by reference — the scoped callbacks capture it.

use std::cell::RefCell;
use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Scope, Table, Value as LuaValue};

use crate::kernel::SyscallCtx;
use crate::ql::ast::Cap;
use crate::services::db::Output;
use crate::value::Value;

/// Raw pointer to a `SyscallCtx` plus trigger metadata.  The pointer is only
/// dereferenced inside `lua.scope()` which runs synchronously on one thread.
pub struct HostData {
    ctx_ptr: *mut SyscallCtx<'static>,
    caps: Cap,
    last_write_span: Option<(i64, i64)>,
}

// SAFETY: the pointer is only dereferenced inside lua.scope() which runs
// synchronously on the calling thread.  The SyscallCtx outlives the scope.
unsafe impl Send for HostData {}
unsafe impl Sync for HostData {}

impl HostData {
    pub fn new(ctx: &mut SyscallCtx<'_>, caps: Cap, last_write_span: Option<(i64, i64)>) -> Self {
        // Extend the pointer's lifetime to `'static` for storage. Raw pointer
        // lifetimes are erased, so clippy sees a no-op cast — keep both arms
        // so the conversion is explicit to the type checker.
        #[allow(clippy::unnecessary_cast)]
        let ctx_ptr = ctx as *mut SyscallCtx<'_> as *mut SyscallCtx<'static>;
        Self {
            ctx_ptr,
            caps,
            last_write_span,
        }
    }

    fn caps(&self) -> Cap {
        self.caps
    }

    fn last_write_span(&self) -> Option<(i64, i64)> {
        self.last_write_span
    }

    /// Re-borrow the kernel syscall context through the raw pointer.
    ///
    /// # Safety
    /// Caller guarantees `ctx_ptr` is valid for the duration of the enclosing
    /// `lua.scope()` and that no concurrent access occurs (single-threaded).
    #[allow(clippy::mut_from_ref)]
    fn ctx(&self) -> &mut SyscallCtx<'static> {
        // SAFETY: valid for the scope's duration (caller guarantees).
        unsafe { &mut *self.ctx_ptr }
    }
}

/// Install the `tau.*` host API within a `lua.scope()`.
///
/// `host` must live for the duration of the scope call (i.e. be created
/// *before* `lua.scope()` and dropped after it returns).
pub fn install<'scope, 'env: 'scope>(
    scope: &'scope Scope<'scope, 'env>,
    lua: &'scope Lua,
    host: &'env RefCell<HostData>,
) -> LuaResult<Table> {
    let caps = host.borrow().caps();
    let tau = lua.create_table()?;

    if caps.contains(Cap::EXEC) {
        let f = scope.create_function(|_, stmt: String| {
            let h = host.borrow_mut();
            let ctx = h.ctx();
            let (_, parsed) = crate::ql::parser::parse(&stmt).map_err(|e| {
                mlua::Error::RuntimeError(crate::ql::format_parse_error(&stmt, e).to_string())
            })?;
            let out = ctx
                .exec(&parsed)
                .map_err(|e| mlua::Error::RuntimeError(format!("{e:?}")))?;
            Ok(output_to_lua(lua, &out))
        })?;
        tau.set("exec", f)?;
    }

    if caps.contains(Cap::AT) {
        let f = scope.create_function(|_, (lens, t): (String, i64)| {
            let h = host.borrow_mut();
            let ctx = h.ctx();
            let stmt = crate::ql::ast::Stmt::At { name: lens, t };
            let out = ctx
                .exec(&stmt)
                .map_err(|e| mlua::Error::RuntimeError(format!("{e:?}")))?;
            Ok(match out {
                Output::Value(Some(v)) => value_to_lua(lua, &v)?,
                _ => LuaValue::Nil,
            })
        })?;
        tau.set("at", f)?;
    }

    if caps.contains(Cap::RANGE) {
        let f = scope.create_function(|_, (lens, s, e): (String, i64, i64)| {
            let h = host.borrow_mut();
            let ctx = h.ctx();
            let stmt = crate::ql::ast::Stmt::Range {
                name: lens,
                start: s,
                end: e,
                filter: None,
                limit: None,
                offset: None,
            };
            let out = ctx
                .exec(&stmt)
                .map_err(|err| mlua::Error::RuntimeError(format!("{err:?}")))?;
            match out {
                Output::Range(segs) => {
                    let tbl = lua.create_table()?;
                    for (i, (s, e, v)) in segs.iter().enumerate() {
                        let row = lua.create_table()?;
                        row.set("s", *s)?;
                        row.set("e", *e)?;
                        row.set("v", value_to_lua(lua, v)?)?;
                        tbl.raw_seti(i + 1, row)?;
                    }
                    Ok(LuaValue::Table(tbl))
                }
                _ => Ok(LuaValue::Nil),
            }
        })?;
        tau.set("range", f)?;

        let f = scope.create_function(|_, (lens, s, e, func): (String, i64, i64, String)| {
            let h = host.borrow_mut();
            let ctx = h.ctx();
            let agg = match func.as_str() {
                "min" => crate::ql::ast::AggFunc::Min,
                "max" => crate::ql::ast::AggFunc::Max,
                "avg" => crate::ql::ast::AggFunc::Avg,
                "sum" => crate::ql::ast::AggFunc::Sum,
                "count" => crate::ql::ast::AggFunc::Count,
                _ => return Err(mlua::Error::RuntimeError(format!("unknown agg: {func}"))),
            };
            let stmt = crate::ql::ast::Stmt::Reduce {
                name: lens,
                start: s,
                end: e,
                func: agg,
            };
            let out = ctx
                .exec(&stmt)
                .map_err(|err| mlua::Error::RuntimeError(format!("{err:?}")))?;
            Ok(match out {
                Output::Value(Some(v)) => value_to_lua(lua, &v)?,
                _ => LuaValue::Nil,
            })
        })?;
        tau.set("reduce", f)?;
    }

    if caps.contains(Cap::LOG) {
        let f = scope.create_function(|_, msg: String| {
            tracing::info!(lua_log = %msg, "lua function log");
            Ok(())
        })?;
        tau.set("log", f)?;
    }

    if caps.contains(Cap::METRIC) {
        let f = scope.create_function(|_, (_name, _val): (String, f64)| Ok(()))?;
        tau.set("metric", f)?;
    }

    if caps.contains(Cap::CLOCK) {
        let f = scope.create_function(|_, ()| {
            let h = host.borrow_mut();
            let ctx = h.ctx();
            Ok(ctx.clock().now_ms())
        })?;
        tau.set("clock", f)?;

        let f = scope.create_function(|_, ms: i64| {
            let h = host.borrow_mut();
            let ctx = h.ctx();
            let now = ctx.clock().now_ms();
            Ok((now - ms, now))
        })?;
        tau.set("clock_window", f)?;

        let f = scope.create_function(|_, ()| -> LuaResult<(mlua::Value, mlua::Value)> {
            let h = host.borrow();
            match h.last_write_span() {
                Some((lo, hi)) => Ok((mlua::Value::Integer(lo), mlua::Value::Integer(hi))),
                None => Ok((mlua::Value::Nil, mlua::Value::Nil)),
            }
        })?;
        tau.set("last_write_span", f)?;
    }

    Ok(tau)
}

fn output_to_lua(lua: &Lua, out: &Output) -> LuaValue {
    match out {
        Output::Empty | Output::Value(None) => LuaValue::Nil,
        Output::Value(Some(v)) => value_to_lua(lua, v).unwrap_or(LuaValue::Nil),
        Output::Range(segs) => match lua.create_table() {
            Ok(tbl) => {
                for (i, (s, e, v)) in segs.iter().enumerate() {
                    if let Ok(row) = lua.create_table() {
                        let _ = row.set("s", *s);
                        let _ = row.set("e", *e);
                        if let Ok(lv) = value_to_lua(lua, v) {
                            let _ = row.set("v", lv);
                        }
                        let _ = tbl.raw_seti(i + 1, row);
                    }
                }
                LuaValue::Table(tbl)
            }
            Err(_) => LuaValue::Nil,
        },
        Output::Names(names) => match lua.create_table() {
            Ok(tbl) => {
                for (i, n) in names.iter().enumerate() {
                    let _ = tbl.raw_seti(i + 1, n.clone());
                }
                LuaValue::Table(tbl)
            }
            Err(_) => LuaValue::Nil,
        },
        _ => LuaValue::Nil,
    }
}

fn value_to_lua(lua: &Lua, v: &Value) -> LuaResult<LuaValue> {
    Ok(match v {
        Value::Int(i) => LuaValue::Integer(*i),
        Value::Float(f) => LuaValue::Number(*f),
        Value::Str(s) => LuaValue::String(lua.create_string(s.as_bytes())?),
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Null => LuaValue::Nil,
    })
}

pub fn literal_to_lua(lua: &Lua, lit: &crate::ql::ast::Literal) -> LuaResult<LuaValue> {
    Ok(match lit {
        crate::ql::ast::Literal::Int(i) => LuaValue::Integer(*i),
        crate::ql::ast::Literal::Float(f) => LuaValue::Number(*f),
        crate::ql::ast::Literal::Str(s) => LuaValue::String(lua.create_string(s.as_bytes())?),
        crate::ql::ast::Literal::Bool(b) => LuaValue::Boolean(*b),
        crate::ql::ast::Literal::Null => LuaValue::Nil,
    })
}

pub fn value_to_lua_owned(lua: &Lua, v: &Value) -> LuaResult<LuaValue> {
    value_to_lua(lua, v)
}

/// Convert a Lua value back into a Tau [`Value`] (used for `CALL FUNCTION` returns).
pub fn lua_to_value(v: LuaValue) -> Option<Value> {
    match v {
        LuaValue::Integer(i) => Some(Value::Int(i)),
        LuaValue::Number(n) => Some(Value::Float(n)),
        LuaValue::String(s) => {
            let s = s.to_str().ok()?.to_string();
            Some(Value::Str(Arc::from(s.as_str())))
        }
        LuaValue::Boolean(b) => Some(Value::Bool(b)),
        _ => None,
    }
}
