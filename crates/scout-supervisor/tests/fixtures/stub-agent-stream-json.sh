#!/bin/sh
# Stand-in agent that emits stream-json shaped output, like
# `claude --print --output-format stream-json --verbose`.
#
# Paced so a test can attach to the live SSE tail part-way through the run
# rather than only after it finishes.

set -e

PROMPT=$(cat)
echo '{"type":"system","subtype":"init","cwd":"/work","model":"claude-opus-5"}'
sleep 0.3
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Reading the repo."}]}}'
sleep 0.3
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}'
sleep 0.3
echo '{"type":"user","message":{"content":[{"type":"tool_result","content":"pub fn x() {}"}]}}'
sleep 0.3

mkdir -p src
printf 'pub fn stub() {}\n' > src/stub.rs
git add src/stub.rs
git -c user.email=stub@example.com -c user.name=Stub commit -q -m "stub implementation"

cat > SPEC.md <<EOF
## Spec: Stream-json stub

### Summary
Produced by the stream-json stub agent.

### Implementation Approach
- Added \`src/stub.rs\`

### Complexity
Simple
EOF

# The final result record. Deliberately does NOT put "type" first: real records
# carry it near the end, and the parser must not depend on key order.
echo '{"subtype":"success","duration_ms":1234,"num_turns":3,"total_cost_usd":0.0421,"usage":{"input_tokens":1200,"output_tokens":340,"cache_read_input_tokens":880,"cache_creation_input_tokens":64},"type":"result"}'
