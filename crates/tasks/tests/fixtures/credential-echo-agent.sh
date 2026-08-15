#!/bin/sh
# Stand-in agent that leaks the way a real one does (#840): it echoes the
# credentialed clone URL the server minted for its VM.
#
# Three shapes, because they take three different routes into the transcript:
# a plain stdout line (what `git remote -v` prints), a stream-json record (what
# the agent narrates), and stderr (what git writes when a fetch fails). The
# token below is a fixture, not a secret — the test asserts it is nowhere in
# the transcript, the API response, or the SQLite files.

set -e

TOKEN='ghp_fixtureTOKEN0123456789abcdefghij'
URL="https://x-access-token:$TOKEN@github.com/o/r.git"

PROMPT=$(cat)

# 1. stdout, verbatim — `git remote -v`.
echo "origin	$URL (fetch)"

# 2. a stream-json record with the URL inside it.
printf '{"type":"assistant","message":{"content":[{"type":"text","text":"cloned %s"}]}}\n' "$URL"

# 3. stderr — git's own failure text.
echo "fatal: could not read from '$URL'" >&2

mkdir -p src
printf 'pub fn stub() {}\n' > src/stub.rs
git add src/stub.rs
git -c user.email=stub@example.com -c user.name=Stub commit -q -m "stub implementation"

# Deliberately no URL in the spec: specs are the Scout's deliverable and are
# not swept, so a leak there would be a different bug with a different fix.
cat > SPEC.md <<'EOF'
## Spec: Credential echo

### Summary
Produced by the credential-echo stub agent.

### Implementation Approach
- Added `src/stub.rs`

### Complexity
Simple
EOF

echo '{"subtype":"success","duration_ms":12,"num_turns":1,"usage":{"input_tokens":10,"output_tokens":2},"type":"result"}'
