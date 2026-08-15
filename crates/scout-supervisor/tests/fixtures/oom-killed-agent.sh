#!/bin/sh
# An agent that dies the way the OOM killer kills one: SIGKILL, no exit code,
# no artifacts. Stands in for the real failure without needing a real memory
# limit — what is under test is that the supervisor reports the death rather
# than flattening it into "-1".

cat > /dev/null
echo "[oom-killed-agent] about to be killed" >&2
kill -9 $$
# Unreachable; here so a shell that somehow survives the kill still fails
# loudly rather than exiting 0.
exit 1
