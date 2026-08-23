#!/bin/sh
# Stand-in builder agent that a test can hold open. See `gated-agent.sh` —
# same contract ($1 is the gate, $1.started says the agent is running), for
# `BUILDER_AGENT_CMD`.
#
# It commits before blocking, so the build a reattached run lands is a real
# branch with real commits in it rather than an empty one.

set -e

GATE="$1"
[ -n "$GATE" ] || { echo "gated-builder-agent: no gate path given" >&2; exit 64; }

PROMPT=$(cat)
echo "[gated-builder] starting in $(pwd)"
echo "[gated-builder] prompt began: $(printf '%s' "$PROMPT" | head -1)" >&2

mkdir -p src
printf 'pub fn built_across_a_restart() {}\n' > src/built.rs
git add src/built.rs
git commit -q -m "Implement the spec"

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
        echo "[gated-builder] gate directory is gone; exiting as orphaned" >&2
        exit 75
    fi
    if [ "$WAITED" -ge 2400 ]; then
        echo "[gated-builder] no gate after 120s; exiting as orphaned" >&2
        exit 75
    fi
    WAITED=$((WAITED + 1))
    sleep 0.05
done

cat > SUMMARY.md <<EOF
Implemented the spec. The server restarted midway through this build.
EOF

echo "[gated-builder] done"
