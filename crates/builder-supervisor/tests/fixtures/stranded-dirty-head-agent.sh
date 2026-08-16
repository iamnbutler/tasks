#!/bin/sh
# The reviewer's case, and the worst outcome in this whole area if the sweep
# runs before the reconciliation.
#
# The implementation is committed on the build branch, HEAD is detached back on
# the base commit, and one more file is written AFTER the detach. A sweep-first
# ordering commits that file onto the base: HEAD becomes base+sweep and the
# branch base+work, the two stand in no ancestor relation at all, the
# divergence arm prefers HEAD, `tip != base` passes the no-commits guard, and
# the build opens a PR containing the sweep and none of the implementation.

set -e

cat > /dev/null
echo "[stranded-dirty-head] starting in $(pwd)"

mkdir -p src
printf 'pub fn implemented() {}\n' > src/implementation.rs
git add src/implementation.rs
git commit -q -m "Implement the spec"

git checkout -q --detach HEAD~1

# `git checkout` pruned src/ on the way back to the base commit.
mkdir -p src
printf 'scratch notes\n' > src/scratch.rs

cat > SUMMARY.md <<'EOF'
Implemented per spec.
EOF

echo "[stranded-dirty-head] done"
