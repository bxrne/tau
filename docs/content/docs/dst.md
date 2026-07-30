+++
title = "DST"
date = 2026-07-30
weight = 55
template = "page.html"
+++

Tau uses [dstest](https://crates.io/crates/dstest) — a deterministic chaos testing framework — to verify container resilience under fault injection. Scripts in the `dst/` directory spin up Docker containers, inject faults (pause, kill, resource deprivation), and assert expected behaviour under chaos.

Same seed = identical fault sequence, making failures reproducible and debuggable.

---

## Installing

```bash
cargo install dstest
```

---

## Running

```bash
# From the repo root
dstest < dst/alive.lua
dstest < dst/smoke.lua
dstest < dst/sweep.lua
```

---

## Scripts

| File | Purpose |
|------|---------|
| `core.lua` | Shared module: spawn, health/metrics assertions, TCP helpers, protocol expectations, coroutine-based orchestrator |
| `alive.lua` | Health + metrics check with single fault injection |
| `smoke.lua` | Full protocol smoke test: AUTH, CREATE, APPEND, DERIVE, point lookups, SHOW LENSES, out-of-range NIL, QUIT |
| `sweep.lua` | Table-driven multi-config orchestrator — spins up multiple containers with different env vars and runs fault rounds against all concurrently via coroutines |

---

## Architecture

`core.lua` is `require`d by every test script. It distils the shared setup — container spawn, key generation, health/metrics checks, TCP connect, protocol command helpers — into one module so test scripts stay declarative.

### Importing core

```lua
package.path = "dst/?.lua;" .. package.path
local core = require("core")
```

### Core API

```lua
local id = core.spawn()                        -- start a Tau container with defaults
local id = core.spawn({ env = { ... } })        -- override spawn opts
core.assert_health(id)                         -- GET /healthz == 200
core.assert_metrics(id)                        -- GET /metrics == 200
local conn = core.connect(id)                   -- TCP to port 7070
local faults = core.faults_new()                -- fault counter
local ok = core.expect_ok(faults)              -- expects "OK" response
ok(conn, "AUTH admin changeme_use_a_strong_password")
core.assert_zero_faults(faults)                -- asserts 0 faults at end
core.step_and_check(id)                         -- inject fault + health check
core.cleanup(id)                                -- clear faults, log done
```

---

## Orchestrator

`core.orchestrate(specs, opts)` runs multiple experiments concurrently using Lua coroutines. One shared fault campaign is injected across all containers; each coroutine's `check` function verifies health after each fault round.

```lua
local report = core.orchestrate({
    {
        name = "baseline",
        spawn_opts = nil,
        setup = function(id, M) M.connect(id) end,
        check = function(id, fault, M)
            if fault.fault ~= "pause" and fault.fault ~= "kill" then
                local r = dstest.http(id, "GET", "/healthz")
                assert(r.status == 200)
            end
        end,
    },
}, { rounds = 10 })
```

Each spec field:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Label for results output |
| `spawn_opts` | no | Override table passed to `core.spawn` |
| `spawn` | no | Custom spawn function (replaces `spawn_opts`) |
| `setup` | no | Called once with `(id, M)` before fault rounds |
| `check` | no | Called each round with `(id, fault_result, M)` |
| `teardown` | no | Called with `(id, M)` after all rounds (defaults to `dstest.clear`) |

Returns `{ passed, failed, total, results }`.

---

## Fault Types

| Fault | Effect |
|-------|--------|
| `pause` | Freeze container (cgroups) |
| `kill` | Kill container (SIGKILL) |
| `deprive:disk` | Throttle disk I/O to 1MB/s |
| `deprive:network` | Disconnect from bridge network |
| `deprive:memory` | Halve memory limit (min 64MB) |
| `deprive:cpu` | Limit CPU to 20% quota |

---

## Configuration

```lua
dstest.config({
    substrate = "docker",
    seed = 42,
    weights = {
        pause = 0.40,
        kill = 0.30,
        ["deprive:network"] = 0.20,
        ["deprive:memory"] = 0.10,
    },
})
```

---

## Determinism

The same seed produces identical fault sequences across runs:

```lua
dstest.config({ seed = 42 })
local r1 = dstest.run_steps(5)

dstest.config({ seed = 42 })
local r2 = dstest.run_steps(5)
-- r1 and r2 are identical
```
