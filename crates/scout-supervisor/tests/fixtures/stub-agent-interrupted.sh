#!/bin/sh
# An agent that ran out of road: notes written, a SPEC.md started but not
# finished, non-zero exit.
#
# This is the shape of the run in #835 (sess_cb6708ed8e9d44569e7bd30ccd526b2e)
# — and before the checkpoint work, that partial SPEC.md was reported as a
# spec and queued for review. The `### Summary` below is deliberately left as
# the prompt template's own placeholder: an agent that wrote the shape and
# then died leaves exactly that.

set -e

cat > /dev/null
echo "[stub-agent-interrupted] exploring" >&2

mkdir -p src
printf 'pub fn half() {}\n' > src/half.rs

cat > NOTES.md <<'EOF'
# Field notes

- The parser lives in `src/parse.rs`, not `src/lib.rs` as the issue says
- `render()` is called from two places, so the signature change is not local
- Still unverified: whether the cache needs invalidating
EOF

cat > SPEC.md <<'EOF'
## Spec: Half a spec

### Summary
One paragraph.

### Implementation Approach
- Touch `src/parse.rs`
EOF

echo "[stub-agent-interrupted] out of room, giving up" >&2
exit 1
