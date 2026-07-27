//! The function registry: stores registered Lua functions, each with its own
//! `mlua::Lua` state (compiled + sandboxed), and invokes them with the
//! capability-gated host bridge.

use std::cell::RefCell;

use mlua::{Function, Lua, Value as LuaValue};

use crate::kernel::SyscallCtx;
use crate::ql::ast::{Cap, Literal, TriggerKind};
use crate::services::db::{ExecError, Output};
use crate::value::Value;

use super::embed::{self, HostData};

struct FunctionState {
    lua: Lua,
    func: Function,
    name: String,
    /// Original Lua body — kept for diagnostics and schema introspection.
    source: String,
    kind: TriggerKind,
    caps: Cap,
    /// Next fire time (ms since epoch) for `SCHEDULE EVERY` functions.
    next_fire_ms: Option<i64>,
}

pub struct Registry {
    funcs: Vec<FunctionState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionVerdict {
    Allow,
    Deny(String),
}

fn lua_err(e: mlua::Error) -> ExecError {
    ExecError::Io(e.to_string())
}

impl Registry {
    pub fn new() -> Self {
        Self { funcs: Vec::new() }
    }

    /// Register (or replace) a function. `now_ms` seeds the first cron fire time.
    pub fn register(
        &mut self,
        name: &str,
        kind: TriggerKind,
        caps: Cap,
        body: &str,
        now_ms: i64,
    ) -> Result<(), String> {
        self.funcs.retain(|f| f.name != name);
        let lua = Lua::new();
        sandbox(&lua)?;
        // Wrap the source in `return function(...) <body> end` so we compile
        // it into a closure without executing the body.  At invocation time
        // we set up `tau` globals and call the closure.
        let wrapped = format!("return function(...) {body} end");
        let func: Function = lua
            .load(&wrapped)
            .set_name(name)
            .eval()
            .map_err(|e| format!("lua compile: {e}"))?;
        let next_fire_ms = match &kind {
            TriggerKind::Cron { every_secs } => {
                let period_ms = every_secs.saturating_mul(1000).max(1);
                Some(now_ms.saturating_add(period_ms))
            }
            _ => None,
        };
        self.funcs.push(FunctionState {
            lua,
            func,
            name: name.to_string(),
            source: body.to_string(),
            kind,
            caps,
            next_fire_ms,
        });
        Ok(())
    }

    pub fn drop_fn(&mut self, name: &str) -> bool {
        let before = self.funcs.len();
        self.funcs.retain(|f| f.name != name);
        self.funcs.len() < before
    }

    pub fn list(&self) -> Vec<String> {
        self.funcs.iter().map(|f| f.name.clone()).collect()
    }

    /// Whether a function with `name` is registered.
    pub fn has(&self, name: &str) -> bool {
        self.funcs.iter().any(|f| f.name == name)
    }

    /// Original Lua source for `name`, if registered.
    pub fn source(&self, name: &str) -> Option<&str> {
        self.funcs
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.source.as_str())
    }

    pub fn invoke_call(
        &self,
        name: &str,
        args: &[Literal],
        ctx: &mut SyscallCtx<'_>,
    ) -> Result<Output, ExecError> {
        if !self.has(name) {
            return Err(ExecError::InvalidExpr(format!("unknown function: {name}")));
        }
        let idx = self
            .funcs
            .iter()
            .position(|f| f.name == name)
            .expect("has() implies present");
        let caps = self.funcs[idx].caps;
        let lua = &self.funcs[idx].lua;
        let func = &self.funcs[idx].func;
        let source = self.source(name).unwrap_or("").to_string();

        let args_tbl = lua.create_table().map_err(lua_err)?;
        for (i, arg) in args.iter().enumerate() {
            let v = embed::literal_to_lua(lua, arg).map_err(lua_err)?;
            args_tbl.raw_seti(i + 1, v).map_err(lua_err)?;
        }
        lua.globals().set("args", args_tbl).map_err(lua_err)?;

        let host = RefCell::new(HostData::new(ctx, caps, None));

        lua.scope(|scope| {
            let tau = embed::install(scope, lua, &host)?;
            lua.globals().set("tau", tau)?;
            let ret: LuaValue = func.call(())?;
            Ok(match embed::lua_to_value(ret) {
                Some(v) => Output::Value(Some(v)),
                None => Output::Empty,
            })
        })
        .map_err(|e| {
            tracing::warn!(function = %name, source = %source, error = %e, "call function failed");
            lua_err(e)
        })
    }

    pub fn invoke_on_write(
        &self,
        lens: &str,
        taus: &[(i64, i64, Value)],
        ctx: &mut SyscallCtx<'_>,
    ) -> Result<(), ExecError> {
        let lo = taus.iter().map(|(s, _, _)| *s).min();
        let hi = taus.iter().map(|(_, e, _)| *e).max();
        let span = lo.zip(hi);

        let matching: Vec<usize> = self.funcs.iter().enumerate()
            .filter(|(_, f)| matches!(&f.kind, TriggerKind::OnWrite { lens: l } if l.as_deref().is_none_or(|n| n == lens)))
            .map(|(i, _)| i).collect();

        for idx in matching {
            let caps = self.funcs[idx].caps;
            let lua = &self.funcs[idx].lua;
            let func = &self.funcs[idx].func;
            let fname = self.funcs[idx].name.clone();
            let source = self.source(&fname).unwrap_or("").to_string();

            let taus_table = lua.create_table().map_err(lua_err)?;
            for (i, (s, e, v)) in taus.iter().enumerate() {
                let row = lua.create_table().map_err(lua_err)?;
                row.set("s", *s).map_err(lua_err)?;
                row.set("e", *e).map_err(lua_err)?;
                row.set("v", embed::value_to_lua_owned(lua, v).map_err(lua_err)?)
                    .map_err(lua_err)?;
                taus_table.raw_seti(i + 1, row).map_err(lua_err)?;
            }
            lua.globals()
                .set("lens", lens.to_string())
                .map_err(lua_err)?;
            lua.globals().set("taus", taus_table).map_err(lua_err)?;

            let host = RefCell::new(HostData::new(ctx, caps, span));

            lua.scope(|scope| {
                let tau = embed::install(scope, lua, &host)?;
                lua.globals().set("tau", tau)?;
                func.call::<()>(())
            })
            .map_err(|e| {
                tracing::warn!(function = %fname, source = %source, error = %e, "on_write trigger failed");
                lua_err(e)
            })?;
        }
        Ok(())
    }

    /// Simple permission hook check without SyscallCtx — evaluates the Lua
    /// source with `caller` and `stmt` globals, minimal `tau.log` only.
    pub fn check_permission_hooks_simple(
        &self,
        caller: &str,
        stmt_text: &str,
    ) -> PermissionVerdict {
        let matching: Vec<usize> = self
            .funcs
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.kind, TriggerKind::OnPermission))
            .map(|(i, _)| i)
            .collect();

        for idx in matching {
            let fname = self.funcs[idx].name.clone();
            let source = self.source(&fname).unwrap_or("").to_string();
            let lua = &self.funcs[idx].lua;
            let func = &self.funcs[idx].func;

            let _ = lua.globals().set("caller", caller.to_string());
            let _ = lua.globals().set("stmt", stmt_text.to_string());

            let verdict = lua.scope(|scope| {
                let tau = lua.create_table()?;
                let log_fn = scope.create_function(|_, msg: String| {
                    tracing::info!(lua_log = %msg, "lua permission hook log");
                    Ok(())
                })?;
                tau.set("log", log_fn)?;
                lua.globals().set("tau", tau)?;

                let result: LuaValue = func.call(())?;
                match result {
                    LuaValue::Boolean(true) | LuaValue::Nil => Ok(PermissionVerdict::Allow),
                    LuaValue::Boolean(false) => {
                        Ok(PermissionVerdict::Deny("denied by permission hook".into()))
                    }
                    _ => Ok(PermissionVerdict::Allow),
                }
            });

            match verdict {
                Ok(PermissionVerdict::Deny(reason)) => return PermissionVerdict::Deny(reason),
                Err(e) => {
                    tracing::warn!(
                        function = %fname,
                        source = %source,
                        error = %e,
                        "permission hook failed"
                    );
                }
                Ok(PermissionVerdict::Allow) => {}
            }
        }
        PermissionVerdict::Allow
    }

    /// Run all `SCHEDULE EVERY` functions whose next fire time is `<= now_ms`.
    /// Updates each fired function's next-fire timestamp. Returns how many ran.
    pub fn invoke_due_cron(
        &mut self,
        now_ms: i64,
        ctx: &mut SyscallCtx<'_>,
    ) -> Result<usize, ExecError> {
        let due: Vec<usize> = self
            .funcs
            .iter()
            .enumerate()
            .filter(|(_, f)| match (&f.kind, f.next_fire_ms) {
                (TriggerKind::Cron { .. }, Some(next)) if next <= now_ms => true,
                (TriggerKind::Cron { .. }, None) => true,
                _ => false,
            })
            .map(|(i, _)| i)
            .collect();

        let mut fired = 0;
        for idx in due {
            let caps = self.funcs[idx].caps;
            let fname = self.funcs[idx].name.clone();
            let source = self.source(&fname).unwrap_or("").to_string();
            let period_ms = match &self.funcs[idx].kind {
                TriggerKind::Cron { every_secs } => every_secs.saturating_mul(1000).max(1),
                _ => continue,
            };
            let lua = &self.funcs[idx].lua;
            let func = &self.funcs[idx].func;

            let host = RefCell::new(HostData::new(ctx, caps, None));
            let result = lua.scope(|scope| {
                let tau = embed::install(scope, lua, &host)?;
                lua.globals().set("tau", tau)?;
                func.call::<()>(())
            });
            if let Err(e) = result {
                tracing::warn!(
                    function = %fname,
                    source = %source,
                    error = %e,
                    "cron function failed"
                );
            } else {
                fired += 1;
            }

            // Advance next fire past now (single fire per tick even if lagging).
            let mut next = self.funcs[idx].next_fire_ms.unwrap_or(now_ms);
            if next <= now_ms {
                next = now_ms.saturating_add(period_ms);
            }
            self.funcs[idx].next_fire_ms = Some(next);
        }
        Ok(fired)
    }
}

fn sandbox(lua: &Lua) -> Result<(), String> {
    let globals = lua.globals();
    for name in ["os", "io", "package", "loadfile", "dofile", "require"] {
        globals
            .set(name, LuaValue::Nil)
            .map_err(|e| format!("sandbox: {e}"))?;
    }
    Ok(())
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kernel;
    use crate::ql::ast::{Cap, TriggerKind};
    use crate::ql::parse;

    fn exec(k: &Kernel, q: &str) -> Output {
        let (_, stmt) = parse(q).expect("parse");
        k.exec(&stmt).expect("exec")
    }

    #[test]
    fn call_function_executes_lua_body() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE LENS out int");
        exec(&k, "CREATE LENS src int");
        exec(&k, "APPEND LENS src 0 10 5");

        let create_fn = "CREATE FUNCTION double CAPS exec AS \"tau.exec('APPEND LENS out 0 1 ' .. (args[1] * 2))\"";
        exec(&k, create_fn);

        exec(&k, "CALL FUNCTION double(21)");

        let (_, at) = parse("AT LENS out 0").unwrap();
        let out = k.exec(&at).unwrap();
        assert_eq!(out, Output::Value(Some(Value::Int(42))));
    }

    #[test]
    fn call_function_returns_lua_value() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE FUNCTION answer CAPS log AS \"return 42\"");
        let out = exec(&k, "CALL FUNCTION answer()");
        assert_eq!(out, Output::Value(Some(Value::Int(42))));
    }

    #[test]
    fn has_and_source_track_registered_functions() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE FUNCTION temp CAPS log AS \"return 1\"");
        // Exercise via CALL / DROP which use has(); source is used on errors
        // and by the registry API under the kernel's func lock path.
        let out = exec(&k, "CALL FUNCTION temp()");
        assert_eq!(out, Output::Value(Some(Value::Int(1))));
        exec(&k, "DROP FUNCTION temp");
        let err = k
            .exec(&parse("CALL FUNCTION temp()").unwrap().1)
            .unwrap_err();
        assert!(matches!(err, ExecError::InvalidExpr(_)));
    }

    #[test]
    fn on_write_trigger_fires_after_append() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE LENS src int");
        exec(&k, "CREATE LENS out int");

        let trigger = "CREATE FUNCTION echo ON WRITE LENS src CAPS exec AS \"tau.exec('APPEND LENS out 0 1 1')\"";
        exec(&k, trigger);

        // Append to src — the trigger should fire and append to out.
        exec(&k, "APPEND LENS src 0 10 42");

        let (_, at) = parse("AT LENS out 0").unwrap();
        let out = k.exec(&at).unwrap();
        assert_eq!(out, Output::Value(Some(Value::Int(1))));
    }

    #[test]
    fn reentrancy_guard_prevents_nested_triggers() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE LENS a int");

        // A trigger that appends to the same lens — should NOT recurse.
        let trigger =
            "CREATE FUNCTION loop ON WRITE LENS a CAPS exec AS \"tau.exec('APPEND LENS a 0 1 1')\"";
        exec(&k, trigger);

        exec(&k, "APPEND LENS a 0 10 99");

        // Should have completed without infinite recursion.
        let (_, at) = parse("AT LENS a 0").unwrap();
        let out = k.exec(&at).unwrap();
        // Newest layer wins: the trigger's append (value 1) is newer.
        assert_eq!(out, Output::Value(Some(Value::Int(1))));
    }

    #[test]
    fn sandbox_blocks_os_access() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");

        // A function that tries to use os — registration succeeds (the body
        // is compiled into a closure, not executed), but calling it fails
        // because os is nil in the sandbox.
        exec(
            &k,
            "CREATE FUNCTION bad CAPS log AS \"os.execute('echo hi')\"",
        );

        let err = k
            .exec(&parse("CALL FUNCTION bad()").unwrap().1)
            .unwrap_err();
        assert!(matches!(err, ExecError::Io(_)), "got {err:?}");
    }

    #[test]
    fn drop_function_removes_it() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE FUNCTION temp CAPS log AS \"\"");
        exec(&k, "DROP FUNCTION temp");

        let err = k
            .exec(&parse("CALL FUNCTION temp()").unwrap().1)
            .unwrap_err();
        assert!(matches!(err, ExecError::InvalidExpr(_)));
    }

    #[test]
    fn show_functions_lists_names() {
        let k = Kernel::new();
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE FUNCTION fn1 CAPS log AS \"\"");
        exec(&k, "CREATE FUNCTION fn2 CAPS log AS \"\"");

        let out = exec(&k, "SHOW FUNCTIONS");
        match out {
            Output::Names(names) => {
                assert!(names.contains(&"fn1".to_string()));
                assert!(names.contains(&"fn2".to_string()));
            }
            _ => panic!("expected Names, got {out:?}"),
        }
    }

    #[test]
    fn create_function_display_roundtrips() {
        let (_, stmt) = parse(
            "CREATE FUNCTION sharpe ON WRITE LENS returns CAPS exec, range, clock AS \"local x = 1\""
        ).unwrap();
        let line = stmt.to_string();
        let (rest, reparsed) = parse(&line).expect("re-parse");
        assert!(rest.trim().is_empty(), "trailing: {rest:?}");
        assert_eq!(reparsed, stmt);
    }

    #[test]
    fn drop_function_display_roundtrips() {
        let (_, stmt) = parse("DROP FUNCTION myfn").unwrap();
        let line = stmt.to_string();
        let (rest, reparsed) = parse(&line).expect("re-parse");
        assert!(rest.trim().is_empty());
        assert_eq!(reparsed, stmt);
    }

    #[test]
    fn scheduled_function_fires_on_tick_cron() {
        let k = Kernel::new();
        k.clock().set_fixed_now_ms(1_000_000);
        exec(&k, "CREATE DATABASE test");
        exec(&k, "CREATE LENS tick int");
        // every 1 second
        exec(
            &k,
            "CREATE FUNCTION counter SCHEDULE EVERY 1 CAPS exec AS \"tau.exec('APPEND LENS tick 0 1 1')\"",
        );
        // Not due yet at t=1_000_000 (next fire is 1_001_000).
        assert_eq!(k.tick_cron().expect("tick"), 0);
        k.clock().set_fixed_now_ms(1_001_000);
        assert_eq!(k.tick_cron().expect("tick"), 1);
        let out = exec(&k, "AT LENS tick 0");
        assert_eq!(out, Output::Value(Some(Value::Int(1))));
        // Not due again until another second passes.
        assert_eq!(k.tick_cron().expect("tick"), 0);
        k.clock().set_fixed_now_ms(1_002_000);
        assert_eq!(k.tick_cron().expect("tick"), 1);
    }

    #[test]
    fn registry_source_preserved() {
        let mut reg = Registry::new();
        reg.register("f", TriggerKind::OnDemand, Cap::LOG, "return 7", 0)
            .unwrap();
        assert!(reg.has("f"));
        assert_eq!(reg.source("f"), Some("return 7"));
        assert!(!reg.has("missing"));
        assert_eq!(reg.source("missing"), None);
    }
}
