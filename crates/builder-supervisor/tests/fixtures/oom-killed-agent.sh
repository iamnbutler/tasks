#!/bin/sh
# An agent that dies the way the OOM killer kills one: SIGKILL, no exit code,
# nothing committed. See the scout-supervisor fixture of the same name.

cat > /dev/null
echo "[oom-killed-agent] about to be killed" >&2
kill -9 $$
exit 1
