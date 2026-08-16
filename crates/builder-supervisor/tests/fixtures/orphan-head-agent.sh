#!/bin/sh
# An agent that leaves HEAD unborn: `git checkout --orphan` points HEAD at a
# branch with no commits, which makes `git rev-parse HEAD` fail outright.
#
# That used to be a fatal `rev-parse head:` — and it fails *before* there is a
# bundle, so the server-side preservation cannot save it either. It is the one
# shape where the work is otherwise unrecoverable.

set -e

cat > /dev/null
echo "[orphan-head] starting in $(pwd)"

mkdir -p src
printf 'pub fn implemented() {}\n' > src/implementation.rs
git add src/implementation.rs
git commit -q -m "Implement the spec"

cat > SUMMARY.md <<'EOF'
Implemented per spec.
EOF

# A fresh start for some experiment, never returned from.
git checkout -q --orphan scratch-branch

echo "[orphan-head] done"
