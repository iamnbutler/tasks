#!/bin/sh
# Minimal stand-in for `claude --print` used in integration tests.
#
# Reads PROMPT.md (ignored), creates a trivial implementation file, and
# writes SPEC.md in the structure the RFC specifies. Exits 0.

set -e

echo "[stub-agent] starting in $(pwd)"
echo "[stub-agent] prompt was:" >&2
head -5 PROMPT.md >&2 || true

# Simulate an implementation change
mkdir -p src
printf 'pub fn stub() {}\n' > src/stub.rs

cat > SPEC.md <<'EOF'
## Spec: Stub implementation

### Summary
Stub agent ran and produced a tiny implementation.

### Implementation Approach
- Added `src/stub.rs` with a no-op function

### Discovered Pitfalls
- None

### Complexity
Simple

### Files Touched
- src/stub.rs
EOF

echo "[stub-agent] done"
