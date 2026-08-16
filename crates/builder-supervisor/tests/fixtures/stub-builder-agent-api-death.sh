#!/bin/sh
# A builder agent whose API connection dies mid-response once, and which then
# finishes the implementation when the supervisor resumes it.
#
# This is where resuming pays for itself: the resume happens inside the VM, so
# the worktree the agent already built survives with the conversation. A
# host-side retry would get a new VM and a fresh clone — and for a Builder that
# worktree IS the implementation. The fixture proves it by committing half the
# work before it dies and the other half after.
#
# The session-id assertion is on this side on purpose: only the agent knows
# which conversation it announced, so only the agent can catch a resume aimed
# at the wrong one.

set -e

STATE="${STUB_STATE:?STUB_STATE must be set}"
SESSION_ID="deadbeef-1111-2222-3333-444444444444"

ATTEMPT=$(cat "$STATE/attempts" 2>/dev/null || echo 0)
ATTEMPT=$((ATTEMPT + 1))
echo "$ATTEMPT" > "$STATE/attempts"

INPUT=$(cat)

if [ "$ATTEMPT" -eq 1 ]; then
  echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\"}"
  mkdir -p src
  printf 'pub fn first_half() {}\n' > src/first_half.rs
  git add src/first_half.rs
  git commit -q -m "First half, before the connection dropped"
  echo "[stub-builder-api-death] connection dropped mid-response" >&2
  echo "{\"subtype\":\"error_during_execution\",\"terminal_reason\":\"api_error\",\"api_error_status\":529,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
  exit 1
fi

case " $* " in
  *" --resume $SESSION_ID "*) ;;
  *)
    echo "[stub-builder-api-death] resumed with the wrong session: $*" >&2
    exit 9
    ;;
esac

case "$INPUT" in
  *"connection to the API dropped"*) ;;
  *)
    echo "[stub-builder-api-death] not a resume prompt: $INPUT" >&2
    exit 9
    ;;
esac

# The proof that the worktree survived: the first half is still on disk and
# still committed, so the agent can build on it instead of redoing it.
test -f src/first_half.rs || { echo "[stub-builder-api-death] worktree was lost" >&2; exit 9; }

printf 'pub fn second_half() {}\n' > src/second_half.rs
git add src/second_half.rs
git commit -q -m "Second half, after the resume"

cat > SUMMARY.md <<EOF
Implemented across a dropped connection; resumed once.
EOF

echo "{\"subtype\":\"success\",\"terminal_reason\":\"completed\",\"api_error_status\":null,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
