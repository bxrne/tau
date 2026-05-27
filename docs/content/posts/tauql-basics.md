+++
title = "TauQL basics"
date = 2026-05-27
description = "A quick tour of the Tau query language and its core statements."
tags = ["tauql", "basics"]
categories = ["guides"]
+++

TauQL is line oriented: one statement per line in, one response line out. The
syntax is intentionally small and focuses on a single lens at a time.

```tauql
CREATE DATABASE demo;
USE DATABASE demo;
CREATE LENS temp float;
APPEND LENS temp 0 50 18.5, 50 100 21.0;
AT LENS temp 25;
RANGE LENS temp 0 100;
REDUCE LENS temp 0 100 USING avg;
```

## Expressions and derived lenses

`DERIVE LENS` stores an expression tree that is evaluated lazily during reads.
Expressions support `+ - * / %`, comparisons, `&&`/`||`, unary `-` and `!`,
parentheses, and aggregation calls.

```tauql
DERIVE LENS smooth AS avg(temp, -60, 0);
DERIVE LENS hot AS temp > avg(temp, -300, 0);
```

Aggregation windows are relative to the evaluation timestamp. For example,
`avg(temp, -60, 0)` at `t=100` aggregates `[40, 100)`.
