#!/usr/bin/env bash
#
# The publish gate for site/. There is no build step, so this is the only
# thing standing between a bad edit and a deployed page.
#
# Three checks, and every one of them reports before the script exits — hence
# `set -uo pipefail` and deliberately **not** `set -e`. A run that stops at
# the first complaint makes you re-run it once per problem.
#
#   1. the disclaimer on the page still matches the README's
#   2. every non-absolute src= resolves to a file that exists
#   3. every non-absolute, non-anchor href= resolves to a file that exists
#
# Check 1 is also implemented in `crates/tasks/tests/site.rs`. That is
# deliberate duplication, not an oversight: this script runs before a
# *deploy*, the test runs before a *merge*, and a drift that only this script
# catches has already landed on `main`. Do not delete either as redundant.
#
# Usage: bash site/check.sh   (never ./site/check.sh — the executable bit is
# not something to depend on surviving a checkout)

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(dirname "$here")"
page="$here/index.html"
readme="$repo/README.md"

fail=0
err() { printf 'site/check.sh: ERROR: %s\n' "$*" >&2; fail=1; }

# --- normalisation -----------------------------------------------------------
#
# Both copies of the disclaimer are prose that is wrapped to fit its own file,
# one as HTML and one as Markdown. A reflow of either must pass and a changed
# word must fail, so both sides are reduced to a bare sequence of words first.
#
# Kept in step with `normalise` in `crates/tasks/tests/site.rs`.
normalise() {
  # 1. drop fenced-code delimiters   2. drop HTML comments   3. drop HTML tags
  # 4. unescape entities   5. drop list/heading/quote markers and Markdown
  #    emphasis   6. collapse all whitespace to single spaces
  perl -0777 -pe '
    s{^[ \t]*```.*$}{}mg;
    s{<!--.*?-->}{}gs;
    s{<[^>]*>}{}gs;
    s{&lt;}{<}g; s{&gt;}{>}g; s{&quot;}{"}g; s{&#39;}{'"'"'}g; s{&amp;}{&}g;
    s{^[ \t]*(?:[-*+]\ |\#+\ |>\ ?)}{}mg;
    s{\*\*}{}g; s{`}{}g;
    s{\s+}{ }gs;
    s{^ }{}; s{ $}{};
  '
}

# Text between `<!-- disclaimer:start -->` and `<!-- disclaimer:end -->`,
# exclusive. Empty output means the markers are not there.
between_markers() {
  awk '/<!-- disclaimer:start -->/{f=1;next} /<!-- disclaimer:end -->/{f=0} f' "$1"
}

# --- 1. the disclaimer has not drifted ---------------------------------------

page_raw="$(between_markers "$page")"
readme_raw="$(between_markers "$readme")"

if [ -z "${page_raw//[[:space:]]/}" ]; then
  err "no disclaimer block in site/index.html.

    The page must carry the README's \`## Read this first\` between
    <!-- disclaimer:start --> and <!-- disclaimer:end -->. The words belong to
    the README; copy them, do not write new ones."
elif [ -z "${readme_raw//[[:space:]]/}" ]; then
  err "no disclaimer block in README.md.

    The README owns these words (#984) and the page copies them. Wrap the
    README's \`## Read this first\` section in <!-- disclaimer:start --> and
    <!-- disclaimer:end --> markers. Until that block exists there is nothing
    for the page to be checked against, and the page does not publish."
else
  page_norm="$(printf '%s\n' "$page_raw" | normalise)"
  readme_norm="$(printf '%s\n' "$readme_raw" | normalise)"
  if [ "$page_norm" != "$readme_norm" ]; then
    err "the disclaimer on the page and the one in the README have drifted.

    They are the same words in two files by contract. Whichever one you
    changed, make the other match; the README is canonical. Word-level diff
    (< README, > page):"
    diff <(printf '%s\n' "$readme_norm" | tr ' ' '\n') \
         <(printf '%s\n' "$page_norm" | tr ' ' '\n') >&2
  fi
fi

# --- 2 & 3. relative references resolve --------------------------------------
#
# The pipeline below runs its loop body in a subshell, so `fail=1` set inside
# it is lost the moment the pipeline ends and the script exits 0 having
# printed every error. Collect the misses to a file and read them back in the
# parent shell instead. Do not "simplify" this into a bare `while read`.
misses="$(mktemp)"
trap 'rm -f "$misses"' EXIT

check_refs() {
  local attr="$1" label="$2" skip_anchors="$3"
  grep -ho "$attr=\"[^\"]*\"" "$page" |
    sed "s/^$attr=\"//; s/\"$//" |
    while IFS= read -r ref; do
      case "$ref" in
        ''|http://*|https://*|//*|mailto:*|data:*) continue ;;
        '#'*) [ "$skip_anchors" = yes ] && continue ;;
      esac
      # Drop any query string or fragment before resolving.
      local path="${ref%%\#*}"
      path="${path%%\?*}"
      [ -z "$path" ] && continue
      [ -e "$here/$path" ] || printf '%s\t%s\n' "$label" "$path" >> "$misses"
    done
}

check_refs src "src" no
check_refs href "href" yes

if [ -s "$misses" ]; then
  while IFS=$'\t' read -r label path; do
    case "$path" in
      img/*.png)
        err "$label=\"$path\" does not exist.

    Screenshots cannot be produced by an agent — app-gpui compiles and tests
    on Linux but does not run there. Take it on a Mac to the spec in
    site/img/MANIFEST.md. This is an error and not a warning on purpose: a
    held deploy leaves the previous page serving, which is the direction this
    should fail in. Do not delete the <figure> to get to green unless you are
    deleting it for good — the page reads fine with two."
        ;;
      *)
        err "$label=\"$path\" does not exist (referenced from site/index.html)."
        ;;
    esac
  done < "$misses"
fi

# -----------------------------------------------------------------------------

if [ "$fail" -eq 0 ]; then
  echo "site/check.sh: ok"
fi
exit "$fail"
