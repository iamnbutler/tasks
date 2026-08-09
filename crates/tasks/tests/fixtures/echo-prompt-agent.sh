#!/bin/sh
# Stand-in agent that copies its ENTIRE stdin prompt into SPEC.md, so a test can
# assert on what the scout was actually told.
#
# The existing crates/scout-supervisor/tests/fixtures/stub-agent.sh only echoes
# the prompt's first line, and it is shared with the supervisor's own tests, so
# it is left alone.
#
# NOTE: this file's own `### Complexity` section must come BEFORE the echoed
# prompt. `parse_complexity` takes the first such section in the produced spec,
# and the echoed prompt contains the template's `Simple | Medium | Complex`
# line, which parse_complexity deliberately rejects as ambiguous. Same trap
# applies to any future prompt-echoing fixture.

set -e

PROMPT=$(cat)
echo "[echo-prompt-agent] starting in $(pwd)"

mkdir -p src
printf 'pub fn stub() {}\n' > src/stub.rs
git add src/stub.rs
git -c user.email=stub@example.com -c user.name=Stub commit -q -m "stub implementation"

cat > SPEC.md <<EOF
## Spec: Echoed prompt

### Complexity
Simple

### Summary
The prompt this scout received is reproduced verbatim below.

### Implementation Approach
- Added \`src/stub.rs\` with a no-op function

### Received prompt

$PROMPT
EOF

echo "[echo-prompt-agent] done"
