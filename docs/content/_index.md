+++
title = "Tau"
sort_by = "date"
paginate_by = 10
template = "index.html"
page_template = "page.html"
+++

<p class="hero__lede">Tau is a <b>bitemporal</b> time-series database built for financial workloads. Every fact keeps <span class="t-valid">when it was true</span> and <span class="t-tx">when you learned it</span>. Corrections are appends — nothing is ever overwritten. <code>AT … AS OF</code> replays exactly what you believed at any past moment, so backtests can't see restated prices. <b>Lua triggers</b> compute Sharpe ratios, rolling stats, and risk signals on write or on a schedule.</p>

<p class="hero__links"><a href="#quickstart">Quickstart</a> · <a href="/docs/examples/#backtesting-point-in-time-correctness">Finance examples</a> · <a href="/docs/scripting/">Lua scripting</a> · <a href="https://github.com/bxrne/tau">GitHub</a></p>

<div class="stack" role="img" aria-label="Two appended layers for the lens px over the same valid-time interval. The newer layer answers AT with 100.0; AT AS OF day one answers from the older layer with 100.4.">
  <div class="stack__head"><span class="t-valid">valid time →</span><span class="t-tx">written_at ↓</span></div>
  <div class="stack__lanes">
    <div class="stack__lane">
      <span class="stack__gen t-tx">day 1</span>
      <span class="stack__bar stack__bar--old">APPEND LENS px 0 3600 100.4<em>the 09:00 bar prints</em></span>
    </div>
    <div class="stack__lane">
      <span class="stack__gen t-tx">day 2</span>
      <span class="stack__bar stack__bar--new">APPEND LENS px 0 3600 100.0<em>the exchange restates it</em></span>
    </div>
    <span class="stack__probe" aria-hidden="true"><i>t&nbsp;=&nbsp;1800</i></span>
  </div>
  <div class="stack__queries">
    <span class="q q--now"><span class="q__prompt">τ:</span> AT LENS px 1800 <span class="q__arrow">→</span> <b>VAL f100.0</b><span class="q__note">today's truth · newest layer wins</span></span>
    <span class="q q--asof"><span class="q__prompt">τ:</span> AT LENS px 1800 <span class="q__asof">AS OF day-1</span> <span class="q__arrow">→</span> <b class="t-tx">VAL f100.4</b><span class="q__note">what you actually traded on</span></span>
  </div>
</div>

<p class="stack__cap">One question, two honest answers. The restated value never deletes the original — it stacks on top, the newest layer wins, and both clocks stay queryable forever. No look-ahead bias. No lost corrections. No mutable state.</p>

## Why Tau for finance

Most time-series stores have one axis of time and mutate in place. A restated price overwrites the original. A backtest sees the corrected data and silently cheats. Tau keeps both axes and mutates nothing.

<div class="axes">
<div class="axes--valid">
<h4>Valid time</h4>
<p>When a fact was true in the world — the half-open interval <code>[start, end)</code> you query with <code>AT</code>, <code>RANGE</code> and <code>REDUCE</code>.</p>
</div>
<div class="axes--tx">
<h4>Transaction time</h4>
<p>When Tau learned it — stamped on every append, wound back with <code>AT … AS OF</code>, and audited with <code>HISTORY</code>.</p>
</div>
</div>


## What that buys you

<ul class="pillars">
<li><b>Corrections are appends.</b> <span>Exchange restatements, corporate actions, recalibrated feeds — the newest layer wins at any overlap; the belief it replaced stays queryable forever.</span></li>
<li><b>Point-in-time backtesting.</b> <span><code>AT LENS px t AS OF &lt;trade-day&gt;</code> gives you exactly the price your strategy saw that day. No look-ahead bias, ever.</span></li>
<li><b>Stack transformations on the same data.</b> <span><code>DERIVE</code> for lazy re-evaluation against the latest corrections. <code>XDERIVE</code> for materialised auto-refreshing signals. Compose a spread, a rolling stat, a risk metric — all over the same base lens, all time-travel-aware.</span></li>
<li><b>Lua triggers.</b> <span>Write Sharpe ratios, rolling stddev, VaR, or position-keeping logic in Lua. Fire on write, on a schedule, or on demand. Sandbox-gated host API: <code>tau.exec</code>, <code>tau.range</code>, <code>tau.clock</code>.</span></li>
<li><b>N-dimensional lenses.</b> <span><code>CREATE LENS quote float AXES (time, instrument, venue)</code> — box-shaped facts, one coordinate per axis. Query a single instrument at a venue, or sweep time across the whole grid.</span></li>
<li><b>Proven correct.</b> <span>Compaction preserves every <code>AT</code>, <code>RANGE</code>, <code>REDUCE</code>, <code>AS OF</code>, and <code>HISTORY</code> result — enforced by property-based tests and deterministic simulation on every build.</span></li>
</ul>

<div class="chips">
<a class="chip" href="/docs/examples/#backtesting-point-in-time-correctness"><b>Backtesting</b> · point-in-time prices</a>
<a class="chip" href="/docs/scripting/"><b>Scripting</b> · Lua triggers &amp; stats</a>
<a class="chip" href="/docs/examples/#iot-sensor-telemetry-with-corrections"><b>IoT</b> · recalibration</a>
</div>


## Quickstart

Install the server and client — release binary, cargo, or Docker:

```bash
# Release binary (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tau-x86_64-linux -o tau
chmod +x tau && sudo mv tau /usr/local/bin/

# …or cargo
cargo install --git https://github.com/bxrne/tau tau tauctl

# …or Docker
docker run -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Start an in-memory server on `127.0.0.1:7070` with `tau`, then drive it from `tauctl`:

```
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE market
τ: CREATE LENS px float
τ: APPEND LENS px 0 3600 100.0, 3600 7200 101.2
τ: AT LENS px 1800
VAL f100
```


## Where next

- [Lua Scripting](/docs/scripting/) — triggers, cron, host API, finance examples (Sharpe, rolling stddev)
- [Examples](/docs/examples/) — copy-pasteable: backtesting, spreads, IoT, observability
- [Tutorial](/docs/tutorial/) — a full correction-and-audit story, end to end
- [TauQL reference](/docs/tauql/) — every statement, the grammar, and the wire protocol
- [How it works](/docs/how-it-works/) — the kernel, layers, compaction, storage and the WAL
- [Simulation testing](/docs/dst/) — the oracle, fault injection, and why seeds reproduce bugs
- [Configuration](/docs/configuration/) — backends, TLS, auth, metrics, and limits

---

<em class="colophon">Open source under the <a href="https://github.com/bxrne/tau/blob/master/LICENSE">Apache 2.0 license</a>. Correctness is enforced by property-based tests, deterministic simulation, and fuzzing — see <a href="/docs/testing/">Testing</a>.</em>
