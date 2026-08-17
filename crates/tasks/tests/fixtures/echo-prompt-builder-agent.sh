#!/bin/sh
# Stand-in builder agent that copies its ENTIRE stdin prompt into SUMMARY.md,
# so a test can assert on what the Builder was actually told.
#
# The Builder counterpart of `echo-prompt-agent.sh`. It works because
# SUMMARY.md comes back on the build row (it is the PR body), which makes the
# prompt assertable from outside the VM — the only seam there is, since the
# prompt is handed to the agent on stdin inside a VM the test never enters.
#
# Unlike the Scout fixture there is no ordering trap here: nothing parses
# SUMMARY.md for a structured field. `verification_report` and
# `summary_accounts_for_review_feedback` both scan it, and both take the first
# marker they recognize — which is exactly what a test asserting on the echoed
# prompt wants to observe.

set -e

PROMPT=$(cat)
echo "[echo-prompt-builder] starting in $(pwd)"

mkdir -p src
printf 'pub fn built() {}\n' > src/built.rs
git add src/built.rs
git commit -q -m "Implement the spec"

cat > SUMMARY.md <<EOF
The prompt this builder received is reproduced verbatim below.

## Received prompt

$PROMPT
EOF

echo "[echo-prompt-builder] done"
