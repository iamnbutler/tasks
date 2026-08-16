#!/bin/sh
# An agent that leaves the implementation on the build branch and its HEAD
# somewhere else: it commits, then detaches back onto the base commit to look
# at something and never comes back.
#
# Reading the head out of the bundle alone would "fix" the mismatch here by
# agreeing on the branch — which is right — but the reconciliation is what has
# to notice that HEAD is the stale one.

set -e

cat > /dev/null
echo "[stranded-head] starting in $(pwd)"

mkdir -p src
printf 'pub fn implemented() {}\n' > src/implementation.rs
git add src/implementation.rs
git commit -q -m "Implement the spec"

# Back to the base commit, detached, to inspect the code as it was.
git checkout -q --detach HEAD~1

cat > SUMMARY.md <<'EOF'
Implemented per spec.
EOF

echo "[stranded-head] done"
