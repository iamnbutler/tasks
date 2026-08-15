#!/bin/sh
# An agent that writes notes and then hangs until its VM is destroyed under it.
#
# The shape of the failure this whole feature exists for: at the deadline the
# host deallocates the VM, so nothing on this disk is ever read again. Only
# what already streamed out as a checkpoint survives.

set -e

cat > /dev/null
printf '# Notes\n\nThe deadline is about to hit; this is all I have.\n' > NOTES.md
echo "[stub-agent-notes-then-hangs] hanging" >&2
sleep 10
