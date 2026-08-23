#!/bin/sh
# Stand-in scout agent that a test can hold open.
#
# The reattachment tests need a run that is *provably* still in flight when the
# first server process dies — otherwise the test could pass by the scout having
# quietly finished first, which proves nothing. So this agent blocks on a file
# only the test creates:
#
#   $1        the gate. The agent waits for it to exist before concluding.
#   $1.started  touched once the agent is running (the test waits for this
#               before killing the first process).
#
# Pass it as `SCOUT_AGENT_CMD="<this script> <gate path>"` — the supervisor
# splits that on whitespace, so the gate rides along as argv[1].

set -e

GATE="$1"
[ -n "$GATE" ] || { echo "gated-agent: no gate path given" >&2; exit 64; }

PROMPT=$(cat)
echo "[gated-agent] starting in $(pwd)"
echo "[gated-agent] prompt began: $(printf '%s' "$PROMPT" | head -1)" >&2

# A committed implementation, exactly like the ordinary stub — so the branch a
# reattached run reports is a real one.
mkdir -p src
printf 'pub fn gated() {}\n' > src/gated.rs
git add src/gated.rs
git -c user.email=gated@example.com -c user.name=Gated commit -q -m "gated implementation"

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
        echo "[gated-agent] gate directory is gone; exiting as orphaned" >&2
        exit 75
    fi
    if [ "$WAITED" -ge 2400 ]; then
        echo "[gated-agent] no gate after 120s; exiting as orphaned" >&2
        exit 75
    fi
    WAITED=$((WAITED + 1))
    sleep 0.05
done

cat > SPEC.md <<EOF
## Spec: Gated implementation

### Summary
A run that was still going when the server restarted.

### Implementation Approach
- Added \`src/gated.rs\`

### Discovered Pitfalls
- None

### Complexity
Simple
EOF

echo "[gated-agent] done"
