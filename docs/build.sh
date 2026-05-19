#!/bin/sh
set -e
mkdir -p changelogs
for crate in tau libtau tauctl; do
  if [ -f "../crates/$crate/CHANGELOG.md" ]; then
    cp "../crates/$crate/CHANGELOG.md" "changelogs/$crate.md"
  fi
done
zola --config zola.toml build
