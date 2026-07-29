# dstest — Chaos Testing for Tau Containers

This directory contains **dstest** scripts for testing Tau's container resilience under fault injection. dstest is a deterministic chaos testing framework that injects faults into Docker containers and verifies system behavior.

## What is dstest?

dstest runs Lua scripts that:
- Spin up containerized services
- Inject faults (pause, kill, resource deprivation)
- Make HTTP requests to verify health
- Assert expected behavior under chaos

Same seed = identical fault sequence, making failures reproducible and debuggable.

## Scripts

| Script | Purpose |
|--------|---------|
| `alive.lua` | Basic health check: spins up Tau, verifies `/healthz` and `/metrics`, injects one fault, confirms recovery |

## Installing

```bash
# From crates.io
cargo install dstest

# From source
cargo install --git https://github.com/bxrne/dstest dstest
```

## Running

```bash
# Run a script
dstest < dst/alive.lua

# Or via stdin
cat dst/alive.lua | dstest

# From repo root during development
cat dst/alive.lua | cargo run --release --bin dstest
```

## Fault Types

| Fault | Effect |
|-------|--------|
| `pause` | Freeze container (cgroups) |
| `kill` | Kill container (SIGKILL) |
| `deprive:disk` | Throttle disk I/O to 1MB/s |
| `deprive:network` | Disconnect from bridge network |
| `deprive:memory` | Halve memory limit (min 64MB) |
| `deprive:cpu` | Limit CPU to 20% quota |

## Configuration

Scripts configure weights for each fault type:

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

## Determinism

The same seed produces identical fault sequences across runs:

```lua
dstest.config({ seed = 42 })
local r1 = dstest.run_steps(5)

dstest.config({ seed = 42 })
local r2 = dstest.run_steps(5)
-- r1 and r2 are identical
```

## Related

- dstest skill: `~/.config/opencode/skills/dstest/SKILL.md`
- dstest crate: linked via Cargo workspace
