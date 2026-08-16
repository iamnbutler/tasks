#!/bin/sh
# An agent whose API connection dies on every attempt: the run exhausts its
# resume budget and ends without a spec.
#
# What must survive is the salvage (the notes written before the first death)
# and a terminal reason that names the transport failure rather than only its
# symptom — "SPEC.md not found" alone reads as a verdict on the exploration.
#
# Like its sibling, this asserts on its own side that a resume named the
# session it announced.

set -e

STATE="${STUB_STATE:?STUB_STATE must be set}"
SESSION_ID="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

ATTEMPT=$(cat "$STATE/attempts" 2>/dev/null || echo 0)
ATTEMPT=$((ATTEMPT + 1))
echo "$ATTEMPT" > "$STATE/attempts"

cat > /dev/null

if [ "$ATTEMPT" -gt 1 ]; then
  case " $* " in
    *" --resume $SESSION_ID "*) ;;
    *)
      echo "[stub-agent-api-death-always] resumed with the wrong session: $*" >&2
      exit 9
      ;;
  esac
fi

echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\"}"
printf '# Notes\n\nFirst finding: the parser is in src/parse.rs.\n' > NOTES.md
echo "[stub-agent-api-death-always] attempt $ATTEMPT: connection dropped" >&2
echo "{\"subtype\":\"error_during_execution\",\"terminal_reason\":\"api_error\",\"api_error_status\":529,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
exit 1
