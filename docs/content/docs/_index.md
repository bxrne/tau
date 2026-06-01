+++
title = "Documentation"
sort_by = "title"
paginate_by = 20
template = "section.html"
page_template = "page.html"
+++

Complete reference for Tau. The data model, query language, storage internals, the deterministic simulation tester and the operational guide.

- [Overview](/docs/overview/). The bitemporal interval model, layers, lenses, compaction and concurrency.
- [How it works](/docs/how-it-works/). Storage internals, WAL, compaction algorithm, design decisions.
- [TauQL Reference](/docs/tauql/). Every statement, operator and response code.
- [DST](/docs/dst/). The deterministic simulation tester. Inspired by FoundationDB and TigerBeetle.
- [Testing](/docs/testing/). Property based tests and unit anchors. How the three layers fit together.
- [Examples](/docs/examples/). Worked queries against the bundled real datasets.
- [Permissions](/docs/permissions/). The CRUDA bitmap, per-statement requirements, grants and wildcards.
- [Configuration](/docs/configuration/). All server flags and environment variables.
- [Containers](/docs/containers/). The Docker stack with Prometheus and Grafana.
- [Roadmap](/docs/roadmap/). What shipped in v0.1 and what v0.2 and v0.3 require.
- [Changelog](/docs/changelog/). Release history.

**Long form.** Start with [Overview](/docs/overview/) for the data model, then [How it works](/docs/how-it-works/) for storage and concurrency internals.
