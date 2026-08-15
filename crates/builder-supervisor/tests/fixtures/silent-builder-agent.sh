#!/bin/sh
# A stand-in for `claude --print` that reproduces the failure #825 is about:
# it talks — at length, on both pipes — and commits nothing, so the build
# fails with "no commits" and the only record of WHY is the transcript.
#
# Exits non-zero so the exit-code line the server writes has something other
# than 0 to say.

set -e

cat > /dev/null   # drain the prompt on stdin

echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Reading the spec."}]}}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}'
echo "could not find the module the spec names; giving up without committing" >&2
echo '{"type":"result","subtype":"error","total_cost_usd":0.01}'

exit 3
