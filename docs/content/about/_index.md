+++
title = "About"
+++

Tau is a time-series engine built on immutable layers of temporal intervals.
New data never overwrites old data; instead it appends a new layer and the
newest layer wins at query time.

The server speaks a line-oriented TCP protocol, while `tauctl` provides an
interactive REPL for quick sessions and CSV loads. For release details, see the
changelog section.
