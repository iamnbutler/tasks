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
while [ ! -e "$GATE" ]; do
    sleep 0.05
done

cat > SUMMARY.md <<EOF
Implemented the spec. The server restarted midway through this build.
EOF

echo "[gated-builder] done"
