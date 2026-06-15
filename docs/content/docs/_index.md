+++
title = "Documentation"
sort_by = "title"
paginate_by = 20
template = "section.html"
page_template = "page.html"
+++

Complete reference for Tau. The data model, query language, storage internals, the deterministic simulation tester and the operational guide.

- [Tutorial](/docs/tutorial/). End-to-end walkthrough: sensor drift correction, layer audit, derived lenses, compaction.
- [Overview](/docs/overview/). The bitemporal interval model, layers, lenses, compaction and concurrency.
- [How it works](/docs/how-it-works/). Storage internals, WAL, compaction algorithm, design decisions.
- [TauQL Reference](/docs/tauql/). Every statement, operator and response code.
- [Testing](/docs/testing/). Property based tests and unit anchors. How the three layers fit together.
- [Permissions](/docs/permissions/). The CRUDA bitmap, per-statement requirements, grants and wildcards.
- [Configuration](/docs/configuration/). All server flags and environment variables.
- [Containers](/docs/containers/). The Docker stack with Prometheus and Grafana, and the Helm chart for Kubernetes.
- [Benchmarks](/docs/benchmarks/). Workloads, the config grid, and reproducible limited-scale results.

**Long form.** Start with [Overview](/docs/overview/) for the data model, then [How it works](/docs/how-it-works/) for storage and concurrency internals.
