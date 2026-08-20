#!/bin/sh
# #1008's shape: an agent that got real work committed AND wrote a summary,
# and then died anyway — the context-exhaustion case, where the branch looks
# complete and is not.
#
# It is the strongest statement of the rule: commits plus a summary plus a
# non-zero exit is still not a success, because nothing in the VM can tell a
# branch that finished from one that stopped.

set -e

cat > /dev/null
echo "[finished-then-died] starting in $(pwd)"

mkdir -p src
printf 'pub fn partial() {}\n' > src/partial.rs
git add src/partial.rs
git commit -q -m "Implement the first spec"

cat > SUMMARY.md <<'SUM'
Implemented the first spec.
SUM

echo '{"type":"result","subtype":"error","terminal_reason":"blocking_limit","result":"Prompt is too long"}'
echo "[finished-then-died] out of context" >&2
exit 1
