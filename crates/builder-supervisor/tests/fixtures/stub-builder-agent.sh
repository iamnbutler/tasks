#!/bin/sh
# Minimal stand-in for `claude --print` used in builder integration tests.
#
# Reads the prompt from stdin, commits an implementation file (like a
# well-behaved agent), leaves a second file UNCOMMITTED (exercising the
# sweep), and writes SUMMARY.md for the PR body. Exits 0.

set -e

PROMPT=$(cat)
echo "[stub-builder] starting in $(pwd)"
echo "[stub-builder] prompt was: $PROMPT" >&2

mkdir -p src
printf 'pub fn built() {}\n' > src/built.rs
git add src/built.rs
git commit -q -m "Implement the spec"

# Left uncommitted on purpose — the supervisor's sweep must catch it.
printf 'forgotten\n' > src/forgotten.rs

FIRST_LINE=$(printf '%s' "$PROMPT" | head -1)
cat > SUMMARY.md <<EOF
Implemented per spec. Prompt began: $FIRST_LINE
EOF

echo "[stub-builder] done"
