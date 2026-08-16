#!/bin/sh
# An agent that tidies its commits into a coherent series before finishing —
# from a DETACHED HEAD, which is the shape behind #891.
#
# It commits twice on the build branch, detaches, squashes those two into one
# and adds a third commit. From the detach onwards refs/heads/<branch> stops
# tracking the work: the supervisor used to report `rev-parse HEAD` while
# bundling the branch, and the server threw the whole build away for the
# mismatch. Four commits, one subject repeated, three distinct tips.

set -e

cat > /dev/null   # the prompt
echo "[history-rewriting] starting in $(pwd)"

mkdir -p src
printf 'pub fn one() {}\n' > src/one.rs
git add src/one.rs
git commit -q -m "Implement the first half"

printf 'pub fn two() {}\n' > src/two.rs
git add src/two.rs
git commit -q -m "Implement the second half"

# Tidy up. `git rebase -i` would do this too; the detach is the point.
git checkout -q --detach
git reset -q --soft HEAD~2
git commit -q -m "Implement the first half"

# `git checkout` prunes directories that became empty, so this has to exist
# again before anything writes back into it.
mkdir -p src
printf 'pub fn three() {}\n' > src/three.rs
git add src/three.rs
git commit -q -m "Implement the rest"

cat > SUMMARY.md <<'EOF'
Implemented per spec, with the history tidied into a coherent series.
EOF

echo "[history-rewriting] done"
