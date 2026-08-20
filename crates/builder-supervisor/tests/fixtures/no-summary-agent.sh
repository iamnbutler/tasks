#!/bin/sh
# An agent that concludes cleanly, commits work, and writes no SUMMARY.md.
#
# The pull request body would then be a list of spec titles under one
# `Implements #NNN` line per task — a claim about the work written by the
# pipeline rather than by the party that did it (#1008).

set -e

cat > /dev/null
echo "[no-summary] starting in $(pwd)"

mkdir -p src
printf 'pub fn built() {}\n' > src/built.rs
git add src/built.rs
git commit -q -m "Implement the spec"

echo "[no-summary] done"
