#!/usr/bin/env bash
#
# One CHANGELOG section, written to stdout.
#
#   bash scripts/changelog.sh <from> <to>     # a section for <from>..<to>
#   bash scripts/changelog.sh --next-version  # the version the next release is
#
# <from> is the previous release tag and may be **empty** — the bootstrap
# case, which logs the whole history and omits the compare link, because there
# is nothing to compare against and the link would 404.
#
# `set -uo pipefail` and deliberately **not** `set -e`, matching site/check.sh:
# a `gh` lookup that fails must degrade to the fallback below, not abort a
# release halfway through writing its own changelog. Every lookup here has a
# no-network answer behind it.
#
# Determinism: CHANGELOG_VERSION / CHANGELOG_DATE / CHANGELOG_HEADLINE override
# what would otherwise be derived, so the Makefile and the tests both get an
# exact section rather than one that depends on today's date.

set -uo pipefail

# --- the walk ---------------------------------------------------------------
#
# `--first-parent` alone is WRONG here, and demonstrably so on this
# repository's own history: `28c879e` ("Merge pull request #758 from
# iamnbutler/feat/mac-app") is reachable from main and is not on main's
# first-parent chain, so the bootstrap section — which logs everything — would
# have shipped with the Mac app missing from it. Structurally it recurs
# whenever a build is merged into another build's branch rather than into the
# trunk, which this pipeline does routinely (it is the same stacking that
# `POST /pull-requests/{n}/retarget` exists for).
#
# So the walk is the full reachable set, and a commit is kept when it is
# either on the first-parent chain (a landing on the trunk) or is itself a
# pull-request merge by subject shape (a landing that happened one level down).
# Everything else — the ordinary second parents of a merge — is dropped by
# being neither.
#
# That widening is what puts the denylist to work: under `--first-parent` the
# back-merge entries below matched nothing on this history. They match now.

# Subjects dropped outright. A stated list rather than a heuristic: anything
# not named here survives, so a new kind of noise shows up in a section and
# gets added deliberately, instead of a cleverness quietly eating a real entry.
is_noise() {
	case "$1" in
	"Merge origin/main into "* | \
		"Merge branch "* | \
		"Merge remote-tracking branch "* | \
		"Sweep: work the agent left uncommitted") return 0 ;;
	esac
	return 1
}

# The two merge-subject shapes that exist on this repository's main: GitHub's
# own, and this pipeline's. Echoes the PR number, or nothing.
pr_number() {
	case "$1" in
	"Merge pull request #"*" from "*)
		n="${1#Merge pull request #}"
		n="${n%% *}"
		;;
	"Merge PR #"*": "*)
		n="${1#Merge PR #}"
		n="${n%%:*}"
		;;
	*) return 1 ;;
	esac
	case "$n" in
	'' | *[!0-9]*) return 1 ;;
	esac
	printf '%s' "$n"
}

# What a kept commit reads as in the changelog.
#
# `Merge PR #N: <title>` already carries the title — strip the prefix.
# `Merge pull request #N from <branch>` does not, but GitHub puts the PR title
# in the commit *body*, which makes the `gh` call the fallback rather than the
# rule: free, offline, deterministic and un-rate-limitable. Verified over this
# repository's last twelve first-parent commits.
entry_for() {
	subject="$1"
	body="$2"
	case "$subject" in
	"Merge PR #"*": "*)
		printf '%s' "${subject#*: }"
		return
		;;
	"Merge pull request #"*" from "*) ;;
	*)
		# This repository's commit subjects are already sentences.
		printf '%s' "$subject"
		return
		;;
	esac

	first_body_line="$(printf '%s\n' "$body" | sed -n '/[^[:space:]]/{p;q;}')"
	if [ -n "$first_body_line" ]; then
		printf '%s' "$first_body_line"
		return
	fi

	number="$(pr_number "$subject")"
	if [ -n "$number" ] && command -v gh >/dev/null 2>&1; then
		# Bounded: this is the fallback for a merge whose body lost its title,
		# and it is reached from `make changelog` on a Mac. The suite also
		# reaches it, inside a Builder VM with no network, where an unbounded
		# `gh` would sit on a DNS lookup rather than degrading.
		title="$(timeout 10 gh pr view "$number" --json title -q .title 2>/dev/null)"
		if [ -n "$title" ]; then
			printf '%s' "$title"
			return
		fi
	fi
	printf '%s' "$subject"
}

# --- the version ------------------------------------------------------------
#
# `0.1.<commit count + 1>`: the changelog commit is inside its own release, so
# the version is one past what HEAD counts today. A single non-merge commit
# adds exactly one to `rev-list --count`, so the speculation is exact.
#
# This is the ONLY place that arithmetic is written — the Makefile's
# PUBLISH_VERSION calls it rather than repeating the expression, because two
# copies of an off-by-one is how one of them gets fixed alone.
#
# It touches neither the network nor `gh`: one `git rev-list`, nothing else.
# A failure prints nothing and exits 0, and `check-publish` refuses on the
# empty result — a make variable that expands to a parse error would break
# every target in the tree, including `make test-ci` inside a Builder VM.
next_version() {
	count="$(git rev-list --count HEAD 2>/dev/null)"
	case "${count:-}" in
	'' | *[!0-9]*) return 0 ;;
	esac
	printf '0.1.%s\n' "$((count + 1))"
}

# --- the section ------------------------------------------------------------

section() {
	from="$1"
	to="$2"

	version="${CHANGELOG_VERSION:-$(next_version)}"
	date="${CHANGELOG_DATE:-$(date -u +%Y-%m-%d)}"
	headline="${CHANGELOG_HEADLINE:-}"

	if [ -n "$from" ]; then
		range="$from..$to"
	else
		range="$to"
	fi

	# %x1f and %x1e are bytes a commit message cannot contain, so a multi-line
	# body can never be mistaken for the start of the next record.
	log="$(git log "$range" --format='%H%x1f%s%x1f%b%x1e' 2>/dev/null)"

	first_parents="$(git rev-list --first-parent "$range" 2>/dev/null)"

	entries=""
	while IFS= read -r -d $'\x1e' record; do
		record="${record#$'\n'}"
		[ -n "$record" ] || continue
		hash="${record%%$'\x1f'*}"
		rest="${record#*$'\x1f'}"
		subject="${rest%%$'\x1f'*}"
		body="${rest#*$'\x1f'}"

		is_noise "$subject" && continue

		# Kept when it landed on the trunk, or when it is a pull-request merge
		# that landed on some other branch first.
		on_trunk=1
		case $'\n'"$first_parents"$'\n' in
		*$'\n'"$hash"$'\n'*) ;;
		*) on_trunk=0 ;;
		esac
		if [ "$on_trunk" -eq 0 ] && ! pr_number "$subject" >/dev/null; then
			continue
		fi

		entry="$(entry_for "$subject" "$body")"
		[ -n "$entry" ] || continue
		# A pull request that landed on another branch and then rode a trunk
		# merge in can render identically twice. One line per landing.
		case $'\n'"$entries" in
		*$'\n'"- $entry"$'\n'*) continue ;;
		esac
		entries="$entries- $entry"$'\n'
	done <<<"$log"

	printf '## v%s — %s\n' "$version" "$date"
	if [ -n "$headline" ]; then
		printf '\n%s\n' "$headline"
	fi
	if [ -n "$entries" ]; then
		printf '\n%s' "$entries"
	fi
	# No compare link for the bootstrap section: there is no earlier tag to
	# compare against and the URL would 404.
	if [ -n "$from" ]; then
		# Deliberately NOT derived from `remote.origin.url`. Inside a Builder
		# VM that remote is the credential broker, and the URL carries a live
		# run lease in its userinfo — deriving the slug from it would write a
		# secret into the one file whose whole purpose is to be published.
		printf '\n[full diff](https://github.com/%s/compare/%s...v%s)\n' \
			"${CHANGELOG_REPO:-iamnbutler/tasks}" "$from" "$version"
	fi
}

usage() {
	printf 'usage: %s <from> <to>\n       %s --next-version\n' "$0" "$0" >&2
}

# `$#` is tested before the value is, because <from> is legitimately empty in
# the bootstrap case and a `case ''` arm would swallow it as a usage error.
if [ "$#" -eq 2 ]; then
	section "$1" "$2"
	exit 0
fi

case "${1:-}" in
--next-version)
	next_version
	;;
-h | --help)
	usage
	exit 0
	;;
*)
	usage
	exit 2
	;;
esac
