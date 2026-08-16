#!/bin/sh
# An agent whose API connection dies mid-response once, and which then finishes
# the job when the supervisor resumes it. The #845 shape.
#
# The attempt counter lives OUTSIDE the workdir ($STUB_STATE, set by the test):
# a marker file inside the clone would show up in files_touched and change what
# the test is measuring.
#
# The session-id assertion below is the load-bearing part. A resume aimed at
# the wrong conversation would be worse than no resume at all — it would look
# like a recovery and behave like a restart — and this fixture is the only
# place that can catch it, because only the agent knows which session it
# announced.

set -e

STATE="${STUB_STATE:?STUB_STATE must be set}"
SESSION_ID="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

ATTEMPT=$(cat "$STATE/attempts" 2>/dev/null || echo 0)
ATTEMPT=$((ATTEMPT + 1))
echo "$ATTEMPT" > "$STATE/attempts"

INPUT=$(cat)

if [ "$ATTEMPT" -eq 1 ]; then
  echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\"}"
  echo "{\"type\":\"assistant\",\"session_id\":\"$SESSION_ID\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Reading the repo.\"}]}}"
  printf '# Notes\n\nFirst finding: the parser is in src/parse.rs.\n' > NOTES.md
  echo "[stub-agent-api-death] connection dropped mid-response" >&2
  # api_error_status is a NUMBER here, exactly as the real CLI writes it.
  echo "{\"subtype\":\"error_during_execution\",\"terminal_reason\":\"api_error\",\"api_error_status\":529,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
  exit 1
fi

case " $* " in
  *" --resume $SESSION_ID "*) ;;
  *)
    echo "[stub-agent-api-death] resumed with the wrong session: $*" >&2
    exit 9
    ;;
esac

# The resume prompt must be the resume prompt — not the task restated, which
# is how a resume silently becomes a restart.
case "$INPUT" in
  *"connection to the API dropped"*) ;;
  *)
    echo "[stub-agent-api-death] not a resume prompt: $INPUT" >&2
    exit 9
    ;;
esac

echo "{\"type\":\"assistant\",\"session_id\":\"$SESSION_ID\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Continuing where I left off.\"}]}}"

mkdir -p src
printf 'pub fn resumed() {}\n' > src/resumed.rs

cat > SPEC.md <<EOF
## Spec: Survived a dropped connection

### Summary
The run continued after the API connection died, in the same worktree.

### Implementation Approach
- Added \`src/resumed.rs\`

### Discovered Pitfalls
- None

### Blockers & Dependencies
None.

### Complexity
Simple

### Notes
Resumed once.
EOF

echo "{\"subtype\":\"success\",\"terminal_reason\":\"completed\",\"api_error_status\":null,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
