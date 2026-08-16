#!/bin/sh
# A builder agent whose API connection dies mid-response, every time, having
# committed nothing. The build fails as "no commits" — the symptom — while the
# class on the event says the run never judged the specs.
#
# Stateless, for the same reason as `api-death-agent.sh`: the host dispatches a
# fresh VM per attempt, so a fixture that asserts on `--resume` would be
# asserting on something only the supervisor can arrange.

set -e

SESSION_ID="bbbbbbbb-cccc-dddd-eeee-ffffffffffff"

cat > /dev/null

echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\"}"
echo "[api-death-builder-agent] connection dropped mid-response" >&2
echo "{\"subtype\":\"error_during_execution\",\"terminal_reason\":\"api_error\",\"api_error_status\":529,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
exit 1
