#!/bin/sh
# A builder agent whose implementation is finished but whose tests fail, and
# which fixes them when the supervisor hands the failure back.
#
# The assertions are on THIS side on purpose: only the agent knows which
# conversation it announced and what it was sent, so only the agent can catch a
# repair round aimed at the wrong session — or one that quietly restated the
# task, which is how a resume becomes a restart.
#
# STUB_NEVER_FIX=1  -> the repair round does not fix it (the suite stays red)
# STUB_MAKE_SLOW=1  -> the repair round trades a red suite for one that hangs,
#                      which is the "an inconclusive re-run must not erase a red
#                      verdict" case

set -e

STATE="${STUB_STATE:?STUB_STATE must be set}"
SESSION_ID="feedfeed-1111-2222-3333-444444444444"

ATTEMPT=$(cat "$STATE/attempts" 2>/dev/null || echo 0)
ATTEMPT=$((ATTEMPT + 1))
echo "$ATTEMPT" > "$STATE/attempts"

INPUT=$(cat)

RESULT="{\"subtype\":\"success\",\"terminal_reason\":\"completed\",\"api_error_status\":null,\"session_id\":\"$SESSION_ID\",\"type\":\"result\"}"
echo "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SESSION_ID\"}"

if [ "$ATTEMPT" -eq 1 ]; then
  mkdir -p src
  printf 'pub fn built() {}\n' > src/built.rs
  # What the project's declared suite fails on.
  printf 'the suite fails while this exists\n' > BROKEN
  git add -A
  git commit -q -m "Implement the spec, and break the suite doing it"
  printf 'First pass. Implemented the spec.\n' > SUMMARY.md
  echo "$RESULT"
  exit 0
fi

# Aimed at the conversation the agent announced, and at no other.
case " $* " in
  *" --resume $SESSION_ID "*) ;;
  *) echo "[red-then-green] resumed with the wrong session: $*" >&2; exit 9 ;;
esac

# A repair prompt, and NOT a restatement of the task: re-sending the specs is
# how a resume silently becomes a restart, so the supervisor must not.
case "$INPUT" in
  *"test suite"*) ;;
  *) echo "[red-then-green] not a repair prompt: $INPUT" >&2; exit 9 ;;
esac
case "$INPUT" in
  *"Spec 1 of 1"*) echo "[red-then-green] the repair round restated the task" >&2; exit 9 ;;
esac

# The proof the worktree survived: the first pass is still on disk and still
# committed, so the agent repairs rather than rebuilding.
test -f src/built.rs || { echo "[red-then-green] worktree was lost" >&2; exit 9; }

if [ -n "$STUB_NEVER_FIX" ]; then
  printf 'Tried and failed to fix the suite.\n' > SUMMARY.md
  git add -A
  git commit -q -m "An attempt that does not fix it"
  echo "$RESULT"
  exit 0
fi

rm -f BROKEN
if [ -n "$STUB_MAKE_SLOW" ]; then
  # Red traded for inconclusive: the suite no longer fails, it never finishes.
  printf 'the suite hangs while this exists\n' > SLOW
fi
printf 'Fixed the failing test.\n' > SUMMARY.md
git add -A
git commit -q -m "Fix the suite"
echo "$RESULT"
