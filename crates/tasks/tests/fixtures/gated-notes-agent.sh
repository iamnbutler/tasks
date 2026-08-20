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
# Bounded, or every test path that ends without opening the gate leaks this
# process — and the supervisor parenting it — until the machine restarts;
# 385 such pairs were found live on the dev host (#1068). Two ways out
# besides the gate: the gate's directory disappearing is the common case
# (the test's tempdir dies with the test, so the gate can never appear
# again), and the deadline is the backstop for everything else. Every green
# test opens the gate within seconds; a stub still waiting at two minutes
# is orphaned by definition.
WAITED=0
while [ ! -e "$GATE" ]; do
    if [ ! -d "$(dirname "$GATE")" ]; then
        echo "[gated-notes-agent] gate directory is gone; exiting as orphaned" >&2
        exit 75
    fi
    if [ "$WAITED" -ge 2400 ]; then
        echo "[gated-notes-agent] no gate after 120s; exiting as orphaned" >&2
        exit 75
    fi
    WAITED=$((WAITED + 1))
    sleep 0.05
done

cat > SPEC.md <<EOF
## Spec: never reached

### Complexity
Simple
EOF

echo "[gated-notes-agent] done"
