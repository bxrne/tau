+++
title = "Documentation"
sort_by = "title"
paginate_by = 20
template = "section.html"
page_template = "page.html"
+++

Complete reference for Tau — the data model, query language, storage internals, and operational guide.

- [Overview](/docs/overview/): the algebraic interval model, layers, lenses, compaction, and concurrency
- [TauQL Reference](/docs/tauql/): every statement, operator, and response code
- [How it works](/docs/how-it-works/): storage internals, WAL, compaction algorithm, design decisions
- [Testing](/docs/testing/): property-based tests, the deterministic simulation tester, and what each layer catches
- [Examples](/docs/examples/): worked queries against the bundled real datasets
- [Configuration](/docs/configuration/): all server flags and environment variables
- [Containers](/docs/containers/): Docker compose stack with Prometheus and Grafana
- [Roadmap](/docs/roadmap/): what shipped in v0.1.0 and what v1.0 requires
- [Changelog](/docs/changelog/): release history

**Tutorials — step by step:**

- [Local](/docs/tutorials/local/): build from source and run the server
- [Docker](/docs/tutorials/docker/): full observability stack in under five minutes
- [Embedded](/docs/tutorials/embedded/): use Tau as a Rust library with no server process
