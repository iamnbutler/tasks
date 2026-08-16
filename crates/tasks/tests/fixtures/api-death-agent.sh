#!/bin/sh
# A scout agent whose API connection dies mid-response, every time, with no
# memory of how often it has been asked — the #845 shape as the *host* sees it.
#
# Deliberately not the scout-supervisor's own `stub-agent-api-death-always.sh`.
# That one counts attempts in $STUB_STATE and asserts that attempt >= 2 carried
# `--resume`, which is right when one supervisor resumes one agent and wrong
# here: the host dispatches a *fresh VM* per attempt, so attempt 2 legitimately
# has no `--resume` and the fixture would exit 9 before writing any
# stream-json. The run would then classify as Silent -> Verdict -> charged,
# and the test would be green while measuring the opposite of what it means to.
#
# Notes are written first so the run has salvage: a transport death is still
# worth the next attempt's while.

set -e

SESSION_ID="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"

cat > /dev/null

echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\"}"
printf '# Notes\n\nFirst finding: the parser is in src/parse.rs.\n' > NOTES.md
echo "[api-death-agent] connection dropped mid-response" >&2
echo "{\"subtype\":\"error_during_execution\",\"terminal_reason\":\"api_error\",\"api_error_status\":529,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
exit 1
