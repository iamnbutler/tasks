#!/bin/sh
# Stand-in scout agent that checkpoints and then blocks forever.
#
# `gated-agent.sh` with one difference: it writes NOTES.md before blocking, so
# a checkpoint provably reaches the host before the test does anything. That is
# what the cancel tests need in order to assert the salvage survives — a run
# that was stopped mid-thought still leaves its leads behind, stamped with why
# the last look was called off.
#
#   $1          the gate. The agent waits for it to exist before concluding —
#               the cancel tests never create it, so the run cannot end on its
#               own and a passing test cannot be a coincidence.
#   $1.started  touched once the notes are written and the agent is blocked.

set -e

GATE="$1"
[ -n "$GATE" ] || { echo "gated-notes-agent: no gate path given" >&2; exit 64; }

PROMPT=$(cat)
echo "[gated-notes-agent] starting in $(pwd)"
echo "[gated-notes-agent] prompt began: $(printf '%s' "$PROMPT" | head -1)" >&2

cat > NOTES.md <<'EOF'
# Field notes

Nothing below is a spec.

- The parser lives in `src/parse.rs`.
- Still checking whether the caller re-enters.
EOF

: > "$GATE.started"
while [ ! -e "$GATE" ]; do
    sleep 0.05
done

cat > SPEC.md <<EOF
## Spec: never reached

### Complexity
Simple
EOF

echo "[gated-notes-agent] done"
