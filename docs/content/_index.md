+++
title = "Tau"
sort_by = "date"
paginate_by = 10
template = "index.html"
page_template = "page.html"
+++

<p class="hero__lede">Tau is a <b>bitemporal</b> time-series database: every fact keeps <span class="t-valid">when it was true</span> and <span class="t-tx">when you learned it</span>. Corrections are appends — nothing is ever overwritten — and <code>AT … AS OF</code> replays exactly what you believed at any past moment.</p>

<p class="hero__links"><a href="#quickstart">Quickstart</a> · <a href="/docs/tutorial/">Tutorial</a> · <a href="/docs/examples/">Examples</a> · <a href="https://github.com/bxrne/tau">GitHub</a></p>

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
    <span class="q q--now"><span class="q__prompt">τ:</span> AT LENS px 1800 <span class="q__arrow">→</span> <b>VAL f100.0</b><span class="q__note">today’s truth · newest layer wins</span></span>
    <span class="q q--asof"><span class="q__prompt">τ:</span> AT LENS px 1800 <span class="q__asof">AS OF day-1</span> <span class="q__arrow">→</span> <b class="t-tx">VAL f100.4</b><span class="q__note">what you actually traded on</span></span>
  </div>
</div>

<p class="stack__cap">One question, two honest answers. The restated value never deletes the original — it stacks on top, the newest layer wins, and both clocks stay queryable forever.</p>

## Two clocks, one fact

Most stores have one axis of time and mutate in place. Tau keeps both axes and mutates nothing.

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
<li><b>Corrections are appends.</b> <span>The newest layer wins at any overlap; the belief it replaced stays queryable forever.</span></li>
<li><b>Time travel survives compaction.</b> <span>Normalisation preserves every transaction-time generation, and is proven query-equivalent by property tests and deterministic simulation on every build.</span></li>
<li><b>Lenses go N-dimensional.</b> <span><code>CREATE LENS grid float AXES (time, region)</code> — box-shaped facts, one coordinate per axis.</span></li>
<li><b>Library or server.</b> <span>Embed the <code>libtau</code> kernel in a Rust process, or run the TCP/TLS server and speak TauQL. Same engine either way.</span></li>
</ul>

<div class="chips">
<a class="chip" href="/docs/examples/#iot-sensor-telemetry-with-corrections"><b>IoT</b> · telemetry &amp; recalibration</a>
<a class="chip" href="/docs/examples/#observability-metrics-and-rollups"><b>Observability</b> · metrics &amp; rollups</a>
<a class="chip" href="/docs/examples/#backtesting-point-in-time-correctness"><b>Backtesting</b> · point-in-time prices</a>
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
τ: CREATE DATABASE demo
τ: CREATE LENS cpu int
τ: APPEND LENS cpu 0 60 45, 60 120 72
τ: AT LENS cpu 30
VAL i45
```


## Where next

- [Tutorial](/docs/tutorial/) — a full correction-and-audit story, end to end
- [Examples](/docs/examples/) — copy-pasteable: IoT recalibration, observability rollups, backtesting
- [TauQL reference](/docs/tauql/) — every statement, the grammar, and the wire protocol
- [How it works](/docs/how-it-works/) — the kernel, layers, compaction, storage and the WAL
- [Simulation testing](/docs/dst/) — the oracle, fault injection, and why seeds reproduce bugs
- [Configuration](/docs/configuration/) — backends, TLS, auth, metrics, and limits

---

<em class="colophon">Open source under the <a href="https://github.com/bxrne/tau/blob/master/LICENSE">Apache 2.0 license</a>. Correctness is enforced by property-based tests, deterministic simulation, and fuzzing — see <a href="/docs/testing/">Testing</a>.</em>
