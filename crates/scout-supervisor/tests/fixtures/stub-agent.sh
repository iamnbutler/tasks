#!/bin/sh
# Minimal stand-in for `claude --print` used in integration tests.
#
# Reads the prompt from stdin (like the real agent), creates a trivial
# implementation file and COMMITS it (like the real agent tends to), then
# writes SPEC.md uncommitted. Exits 0.

set -e

PROMPT=$(cat)
echo "[stub-agent] starting in $(pwd)"
echo "[stub-agent] prompt was: $PROMPT" >&2

# Simulate an implementation change, committed by the agent.
mkdir -p src
printf 'pub fn stub() {}\n' > src/stub.rs
git add src/stub.rs
git -c user.email=stub@example.com -c user.name=Stub commit -q -m "stub implementation"

FIRST_LINE=$(printf '%s' "$PROMPT" | head -1)
cat > SPEC.md <<EOF
## Spec: Stub implementation

### Summary
Stub agent ran against prompt: $FIRST_LINE

### Implementation Approach
- Added \`src/stub.rs\` with a no-op function

### Discovered Pitfalls
- None

### Complexity
Simple

### Files Touched
- src/stub.rs
EOF

echo "[stub-agent] done"
