#!/bin/sh
# An agent that keeps notes and never writes a spec, so the checkpoint watcher
# has something to observe twice.
#
# The `sleep 2`s are load-bearing: the test runs the watcher at a 1s interval,
# and each sleep gives it a clean 1s margin to observe that snapshot before the
# next write. If this test ever flakes, WIDEN the sleeps — do not shorten the
# interval, which just moves the race somewhere less visible.

set -e

cat > /dev/null
echo "[stub-agent-notes-only] starting" >&2

printf '# Notes\n\nFirst finding: the bug is in src/parse.rs.\n' > NOTES.md
sleep 2

printf '# Notes\n\nFirst finding: the bug is in src/parse.rs.\nSecond finding: the clock is a lie.\n' > NOTES.md
sleep 2

echo "[stub-agent-notes-only] done, without a spec" >&2
