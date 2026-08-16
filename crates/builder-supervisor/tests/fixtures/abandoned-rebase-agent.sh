#!/bin/sh
# An agent that starts a rebase, hits a conflict and never finishes it — the
# trap in "prefer HEAD when diverged".
#
# git parks HEAD on a partial replay and keeps refs/heads/<branch> at the
# complete pre-rebase tip until the rebase finishes, so ancestry sees an
# ordinary divergence and the naive rule ships the partial history.

set -e

cat > /dev/null
echo "[abandoned-rebase] starting in $(pwd)"

BRANCH=$(git rev-parse --abbrev-ref HEAD)
BASE=$(git rev-parse HEAD)

mkdir -p src
printf 'pub fn implemented() {}\n' > src/implementation.rs
printf 'ours\n' > src/conflict.rs
git add -A
git commit -q -m "Implement the spec"

# Somebody else's change to the same file, off to one side …
git checkout -q --detach "$BASE"
mkdir -p src
printf 'theirs\n' > src/conflict.rs
git add -A
git commit -q -m "A change to the same file"
SIDE=$(git rev-parse HEAD)

# … and a rebase onto it, which stops on the conflict and leaves HEAD
# detached mid-replay.
git checkout -q "$BRANCH"
git rebase "$SIDE" > /dev/null 2>&1 || echo "[abandoned-rebase] rebase stopped on a conflict"

cat > SUMMARY.md <<'EOF'
Implemented per spec.
EOF

echo "[abandoned-rebase] done"
