+++
title = "Tau"
sort_by = "date"
paginate_by = 10
template = "index.html"
page_template = "page.html"
+++

<h1 class="hero__title">A database that never forgets what it used to believe.</h1>

<p class="hero__lede">Tau is <b>bitemporal</b>: every fact records when it was true <span class="astime">and when you learned it</span>. Corrections are just appends — nothing is overwritten — and <code>AT … AS OF</code> replays any past belief, exactly.</p>

<div class="term">
<span class="term__line"><span class="term__p">τ:</span> APPEND LENS px 0 3600 100.4         <span class="term__c"># the 09:00 bar prints</span></span>
<span class="term__line"><span class="term__p">τ:</span> AT LENS px 1800                      <span class="term__r">→ VAL f100.4</span></span>
<span class="term__line"> </span>
<span class="term__line"><span class="term__p">τ:</span> APPEND LENS px 0 3600 100.0         <span class="term__c"># next day — the exchange restates it</span></span>
<span class="term__line"><span class="term__p">τ:</span> AT LENS px 1800                      <span class="term__now">→ VAL f100.0    ← today's truth</span></span>
<span class="term__line"><span class="term__p">τ:</span> AT LENS px 1800 AS OF &lt;trade-day&gt;   <span class="term__asof">→ VAL f100.4    ← what you actually traded on</span></span>
</div>

<p class="term__cap">One question, <span class="astime">two answers</span> — the price you see now, and the price you saw then. No lookahead bias, no shadow tables, no rebuild.</p>

<div class="cta-row">
<a class="cta" href="#quickstart">Quickstart</a>
<a class="cta cta--ghost" href="/docs/tutorial/">Read the tutorial</a>
<a class="cta cta--ghost" href="/docs/examples/">Browse examples</a>
</div>

---

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

---

## What that buys you

<ul class="pillars">
<li><b>Corrections are appends.</b> <span>Overwrite nothing. The newest layer wins at any overlap; the belief it replaced stays queryable forever.</span></li>
<li><b>Time-travel that survives compaction.</b> <span>Normalisation preserves every transaction-time generation — a sweep line in one dimension, orthotope subtraction in N — and is proven query-equivalent by property tests and deterministic simulation on every build.</span></li>
<li><b>Lenses go N-dimensional.</b> <span><code>CREATE LENS grid float AXES (time, region)</code> — box-shaped facts, queried one coordinate per axis.</span></li>
<li><b>Library or server.</b> <span>Embed <code>libtau</code> in a Rust process, or run the TCP/TLS server and speak TauQL. Same engine either way.</span></li>
</ul>

<div class="chips">
<a class="chip" href="/docs/examples/#iot-sensor-telemetry-with-corrections"><b>IoT</b> · telemetry &amp; recalibration</a>
<a class="chip" href="/docs/examples/#observability-metrics-and-rollups"><b>Observability</b> · metrics &amp; rollups</a>
<a class="chip" href="/docs/examples/#backtesting-point-in-time-correctness"><b>Backtesting</b> · point-in-time prices</a>
</div>

---

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

From here: the [tutorial](/docs/tutorial/) walks a full correction-and-audit story, the [examples](/docs/examples/) are copy-pasteable per use case, and [how it works](/docs/how-it-works/) opens up storage, the WAL and compaction.

---

*Open source under the [Apache&nbsp;2.0 license](https://github.com/bxrne/tau/blob/master/LICENSE). Correctness is enforced by property-based tests, a deterministic simulation tester, and fuzzing — see [Testing](/docs/testing/).*
