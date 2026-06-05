+++
title = "Tau"
sort_by = "date"
paginate_by = 10
template = "index.html"
page_template = "page.html"
+++

**A bitemporal time series database. Corrections, restatements and out of order arrivals are first class. Old values are never overwritten and every query returns the right answer at any point in time.**

1. **Corrections are first class.** Every append is an immutable layer. The newest layer wins where layers overlap. Old values stay on disk.
2. **Compaction is a provable normalisation.** A sweep line algorithm collapses N layers into one canonical layer. Query equivalent before and after. Checked by property based tests on every build.
3. **TauQL.** A tiny query language. One statement in, one response line out. Derived lenses compose as lazy closures. Rolling window aggregations are first class expressions.
4. **Library or server.** Embed `libtau` in a Rust process or run the standalone TCP server. Same engine.

---

## A correction in practice

Time series data is not static. Sensors drift. Prices get restated. Audit records get amended. Tau models this directly. Values live in intervals `[start, end)` that tile without gaps or overlap. A correction is just another append. The newest layer wins. Old data is never touched.

```
CREATE DATABASE sensors
CREATE LENS temperature float
APPEND LENS temperature 0 3600 18.5, 3600 7200 21.0
AT LENS temperature 1800        -> VAL f18.5
RANGE LENS temperature 0 7200  -> RANGE 2; 0:3600:f18.5; 3600:7200:f21
REDUCE LENS temperature 0 7200 USING avg  -> VAL f19.75

APPEND LENS temperature 0 3600 20.0
AT LENS temperature 1800        -> VAL f20
RANGE LENS temperature 0 7200  -> RANGE 2; 0:3600:f20; 3600:7200:f21
REDUCE LENS temperature 0 7200 USING avg  -> VAL f20.5
```

The second `APPEND` is a correction. The original layer is still on disk. The new query returns the corrected value. The aggregate reflects the correction.

---

## Get started

Install the server and client:

```bash
# Release binary (Linux x86_64)
curl -fsSL https://github.com/bxrne/tau/releases/latest/download/tau-x86_64-linux -o tau
chmod +x tau && sudo mv tau /usr/local/bin/

# Or via cargo
cargo install --git https://github.com/bxrne/tau tau

# Or via Docker
docker pull ghcr.io/bxrne/tau:latest
docker run -p 7070:7070 ghcr.io/bxrne/tau:latest
```

Start an in-memory server on `127.0.0.1:7070`:

```bash
tau
```

Connect with `tauctl`:

```bash
tauctl
τ: connect demo 127.0.0.1:7070
τ: CREATE DATABASE demo
τ: CREATE LENS cpu int
τ: APPEND LENS cpu 0 60 45, 60 120 72
τ: AT LENS cpu 30
VAL i45
```


---

## Read more

- [Tutorial](/docs/tutorial/). End-to-end walkthrough: sensor drift correction.
- [Overview](/docs/overview/). The data model.
- [How it works](/docs/how-it-works/). Storage, WAL, compaction, concurrency.
- [TauQL reference](/docs/tauql/). Every statement and operator.
- [DST](/docs/dst/). `libdst` framework and Tau `dst` driver.
- [Testing](/docs/testing/). Unit tests, Hegel PBT, DST, and fuzzing.
- [Configuration](/docs/configuration/). All flags and environment variables.

---

*Tau is open source under the [Apache 2.0 license](https://github.com/bxrne/tau/blob/master/LICENSE).*
