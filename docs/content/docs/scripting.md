+++
title = "Lua Scripting"
date = 2026-07-27
template = "page.html"
+++

Tau embeds **LuaJIT** via `mlua`. You write functions in Lua, register them with `CREATE FUNCTION`, and they fire on triggers, on a schedule, or on demand. The host API is capability-gated: each function declares which `tau.*` calls it may use, and the Lua environment is sandboxed — no `os`, `io`, or `require`.

This is the computation layer that makes Tau expressive for finance: rolling Sharpe ratios, stddev, VaR, position-keeping, compliance gates — all computed inside the kernel, against the same bitemporal data model that powers `AT … AS OF`.

---

## Grammar

```
CREATE FUNCTION <name>
  [ON WRITE [LENS <ident>]]
  [SCHEDULE EVERY <secs>]
  [CAPS <cap>+]
  AS "<lua source>"

DROP FUNCTION <name>
CALL FUNCTION <name> [(<literal> …)]
SHOW FUNCTIONS
```

One trigger kind per function (`ON WRITE`, `SCHEDULE EVERY`, or omitted for on-demand). `CAPS` declares the host-API capabilities the function may use — if omitted, defaults to `log` only.

---

## Trigger kinds

| Kind | Syntax | Fires when |
|------|--------|-----------|
| Write trigger | `ON WRITE` or `ON WRITE LENS x` | After an `APPEND` (any lens, or a specific one) |
| Scheduled | `SCHEDULE EVERY <secs>` | Periodically from the host loop |
| Permission hook | *(see below)* | Before a statement executes, can allow or deny |
| On-demand | *(omit trigger clause)* | When `CALL FUNCTION name(...)` is issued |

### Write triggers

Fire after a successful `APPEND` / `BATCH APPEND` / `COPY`. The Lua function receives `(db, lens, taus)` where `taus` is a Lua table of `{s, e, v}` triples. `tau.last_write_span()` returns `[lo, hi)` — the bounding interval of the written taus — so the function can recompute only the affected span.

```sql
CREATE FUNCTION recompute_spread ON WRITE LENS aapl CAPS exec, range, clock
AS "
  local lo, hi = tau.last_write_span()
  local aapl = tau.range('aapl', lo, hi)
  local msft = tau.range('msft', lo, hi)
  -- ... compute spread and append ...
  tau.exec(('APPEND LENS spread %d %d %f'):format(lo, hi, spread))
"
```

**Reentrancy guard:** triggers do not fire inside other triggers. If a write trigger itself `APPEND`s, that append does not re-fire any `ON WRITE` triggers. This prevents infinite recursion and keeps borrow scopes finite.

### Scheduled functions (cron)

```sql
CREATE FUNCTION daily_rollup SCHEDULE EVERY 86400 CAPS exec, range, clock
AS "
  local s, e = tau.clock_window(86400000)   -- last 24h
  local rows = tau.range('trades', s, e)
  local vol = 0
  for _, r in ipairs(rows) do vol = vol + r.v end
  tau.exec(('APPEND LENS daily_volume 0 %d %d'):format(e, vol))
"
```

The host loop fires a `Tick` every ~100ms; any scheduled function whose next-fire time has passed runs with a syscall context. The virtual `Clock` makes this deterministic in simulation.

### Permission hooks

A permission hook is consultative: it returns `Allow` or `Deny(reason)`, and the kernel still applies normal CRUDA checks afterwards. Both must pass for the statement to execute.

```sql
CREATE FUNCTION trade_limit CAPS exec, log
AS "
  -- Deny trades over 1M units for user 'bob'
  return function(caller, stmt)
    if caller == 'bob' and stmt:match('APPEND') then
      return false, 'trade limit exceeded'
    end
    return true
  end
"
```

### On-demand (`CALL FUNCTION`)

```sql
CREATE FUNCTION double CAPS exec
AS "
  local v = ...
  tau.exec(('APPEND LENS out 0 1 %d'):format(v * 2))
"

τ: CALL FUNCTION double(21)
→ OK
```

---

## Host API (`tau.*`)

Every `tau.*` call is gated by a declared capability. A function that tries to call `tau.exec` without the `exec` cap gets a clean Lua error.

| Call | Capability | Returns | Description |
|------|-----------|---------|-------------|
| `tau.exec(stmt)` | `exec` | `nil` or a value | Run a TauQL statement string (mutation or read) |
| `tau.at(lens, t)` | `at` | `nil` / int / float / str / bool | Point lookup — read-only fast path |
| `tau.range(lens, s, e)` | `range` | `{{s,e,v}, …}` | Range scan as a Lua table of triples |
| `tau.reduce(lens, s, e, func)` | `range` | number | Aggregate: `min`/`max`/`avg`/`sum`/`count` |
| `tau.log(msg)` | `log` | `nil` | Structured log into the kernel's tracing sink |
| `tau.metric(name, val)` | `metric` | `nil` | Record a custom metric |
| `tau.clock()` | `clock` | `int` | Current virtual time (ms since epoch) |
| `tau.clock_window(ms)` | `clock` | `int, int` | `[now-ms, now)` — convenient rolling-window bounds |
| `tau.last_write_span()` | `clock` | `int, int` | `[lo, hi)` of the taus that fired an `ON WRITE` trigger |
| `tau.faults()` | `faults` | table | Armed fault-injection state (read-only, for sim) |

### Output conversion

`tau.exec` / `tau.at` / `tau.range` return Lua-native values:

| TauQL output | Lua value |
|-------------|-----------|
| `Output::Empty` | `nil` |
| `Output::Value(Some(v))` | scalar (`number`, `string`, `boolean`, or `nil`) |
| `Output::Value(None)` | `nil` |
| `Output::Range(segs)` | `{{s=s, e=e, v=v}, …}` |
| `Output::Names(names)` | `{"name", …}` |

---

## Sandboxing

The Lua `_G` is stripped at function-compile time. Removed globals:

- `os` — no filesystem, no environment, no `execute`
- `io` — no file I/O
- `loadfile`, `dofile` — no loading external code
- `require`, `package` — no module system
- `debug` — no debug library

The only I/O a function can perform is through the `tau.*` host API. A function that tries `os.execute("rm -rf /")` gets `attempt to index a nil value (global 'os')` — a clean Lua error, not a shell.

The available standard library: `string`, `table`, `math`, `coroutine`, `pairs`, `ipairs`, `tostring`, `tonumber`, `type`, `pcall`, `error`, `assert`, `select`, `unpack`, `next`, `setmetatable`, `getmetatable`, `rawget`, `rawset`, `rawequal`, `rawlen`.

---

## Persistence

`CREATE FUNCTION` and `DROP FUNCTION` implement `Display` and persist in the schema WAL alongside `DERIVE`/`XDERIVE`/`SET TTL`. On restart, the schema section replays, recompiling each function's Lua source into a fresh `mlua::Lua` state. Functions survive crashes exactly like derived lenses do.

---

## Loading Lua from a file (`import lua`)

The `tauctl` client provides an `import lua` meta-command for loading Lua source from a file, so you can develop and version your functions as `.lua` files instead of inlining them on the command line.

```
import lua <name> <path> [trigger clause] [CAPS ...]
```

The client reads the file, joins newlines into spaces (the wire protocol is line-oriented), escapes `"` as `\"` and `\` as `\\` (so Lua source using `"` for string literals is safe), and sends a single `CREATE FUNCTION` statement.

```
# sharpe.lua:
#   local s, e = tau.clock_window(86400000)
#   local rows = tau.range('returns', s, e)
#   ...

τ: import lua sharpe_24h sharpe.lua ON WRITE LENS returns CAPS exec,range,clock
→ OK
```

### String escape sequences

TauQL string literals support two escape sequences: `\"` (double-quote) and `\\` (backslash). This is needed for embedding Lua source — which commonly uses `"` for string literals — as a `CREATE FUNCTION` body. All other characters (including `\n`, which the `import lua` command replaces with a space) are literal.

```
CREATE FUNCTION f AS "local s = \"hello\""
```

In Lua, this function body is `local s = "hello"`.

---

## Finance examples

### Rolling Sharpe ratio

Compute a 24h rolling Sharpe ratio on every write to the returns lens:

```sql
CREATE LENS returns float
CREATE LENS sharpe float

CREATE FUNCTION sharpe_24h ON WRITE LENS returns CAPS exec, range, clock
AS "
  local s, e = tau.clock_window(86400000)
  local rows = tau.range('returns', s, e)
  local n, sum, sum2 = 0, 0.0, 0.0
  for _, r in ipairs(rows) do
    n = n + 1; sum = sum + r.v; sum2 = sum2 + r.v * r.v
  end
  if n < 2 then return end
  local mean = sum / n
  local variance = sum2 / n - mean * mean
  local sd = math.sqrt(math.max(variance, 0))
  local sh = (sd > 0) and (mean / sd) or 0.0
  tau.exec(('APPEND LENS sharpe %d %d %f'):format(s, e, sh))
"
```

### Rolling standard deviation

Pure Lua, no native stats helpers needed:

```sql
CREATE LENS px float
CREATE LENS stddev float

CREATE FUNCTION rolling_stddev ON WRITE LENS px CAPS exec, range, clock
AS "
  local lo, hi = tau.last_write_span()
  local window = 3600000  -- 1h in ms
  local s, e = tau.clock_window(window)
  local rows = tau.range('px', s, e)
  local n, sum, sum2 = 0, 0.0, 0.0
  for _, r in ipairs(rows) do
    n = n + 1; sum = sum + r.v; sum2 = sum2 + r.v * r.v
  end
  if n < 2 then return end
  local mean = sum / n
  local sd = math.sqrt(math.max(sum2 / n - mean * mean, 0))
  tau.exec(('APPEND LENS stddev %d %d %f'):format(lo, hi, sd))
"
```

### Position-keeping on trade writes

Maintain a running position lens from a trades lens:

```sql
CREATE LENS trades int
CREATE LENS position int

CREATE FUNCTION update_position ON WRITE LENS trades CAPS exec, range, clock
AS "
  local lo, hi = tau.last_write_span()
  local rows = tau.range('trades', lo, hi)
  local delta = 0
  for _, r in ipairs(rows) do delta = delta + r.v end
  local cur = tau.at('position', hi - 1) or 0
  tau.exec(('APPEND LENS position %d %d %d'):format(lo, hi, cur + delta))
"
```

### End-of-day volume rollup

Scheduled function, fires every 86400 seconds:

```sql
CREATE LENS trades int
CREATE LENS daily_volume int

CREATE FUNCTION eod_volume SCHEDULE EVERY 86400 CAPS exec, range, clock
AS "
  local s, e = tau.clock_window(86400000)
  local rows = tau.range('trades', s, e)
  local vol = 0
  for _, r in ipairs(rows) do vol = vol + r.v end
  tau.exec(('APPEND LENS daily_volume 0 %d %d'):format(e, vol))
"
```

### N-dimensional grid + Lua

Store quotes in a 3-axis lens (time, instrument, venue), compute a VWAP per instrument via Lua:

```sql
CREATE LENS quote float AXES (time, instrument, venue)

CREATE FUNCTION vwap ON WRITE LENS quote CAPS exec, range, clock
AS "
  -- For each instrument (1..N), compute VWAP over the last hour
  for inst = 1, 100 do
    local rows = tau.range('quote', tau.clock_window(3600000))
    -- filter to this instrument (venue axis collapsed in range)
    -- ... compute vwap ...
    tau.exec(('APPEND LENS vwap_%d %d %d %f'):format(inst, lo, hi, vwap))
  end
"
```

---

## Management

```sql
SHOW FUNCTIONS
→ NAMES 3; sharpe_24h; rolling_stddev; eod_volume

DROP FUNCTION sharpe_24h
→ OK
```

---

## Testing

Lua functions are covered by two testing layers:

1. **Unit tests** (`libtau/src/func/`) — sandboxing (assert `os` is nil), `CALL FUNCTION` return values, `tau.exec` round-trips, reentrancy guard.
2. **DST** (`crates/dst/`) — canned functions with known semantics are dual-simulated: the target runs the Lua, the oracle computes the expected effect independently. See [DST](/docs/dst/). 

See [Testing](/docs/testing/) for the full strategy.
