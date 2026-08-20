# Container image pipeline. These targets cross-compile macOS -> Linux and are
# the only place the cross linker is pinned. Prereqs (make check-toolchain):
#   brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu
#   rustup target add aarch64-unknown-linux-gnu
#   apple/container CLI (`container system start` before building images)
#
# Plain `cargo build` / `cargo test --workspace` are native builds and need
# none of the above — including on an aarch64 Linux host, where the target
# triple below happens to be the host triple.

LINUX_TARGET := aarch64-unknown-linux-gnu
CROSS_LINKER := $(LINUX_TARGET)-gcc
# Cargo's `[target.*]` config keys are host-blind, so the pin has to live here
# rather than in .cargo/config.toml: on an aarch64 Linux host that triple is
# the host triple and the macOS-only linker above does not exist. The env var
# name is derived from the triple by cargo's own uppercase/underscore rule, so
# changing LINUX_TARGET can't silently desync the two.
CROSS_ENV := CARGO_TARGET_$(shell echo $(LINUX_TARGET) | tr 'a-z-' 'A-Z_')_LINKER=$(CROSS_LINKER)
SCOUT_BIN := target/$(LINUX_TARGET)/release/scout-supervisor
BUILDER_BIN := target/$(LINUX_TARGET)/release/builder-supervisor
VM_SUPERVISOR_BIN := target/$(LINUX_TARGET)/release/supervisor

# Where cargo puts debug binaries, honouring an environment override.
# abspath because tests run with cwd set to their own package.
#
# Agent worktrees (.claude/worktrees/*) default to one shared target directory
# instead of a fresh empty target/ each: external deps compile once ever, and
# cargo keys workspace crates by package path, so two worktrees' artifacts
# coexist in the directory rather than thrash — measured, not assumed (a
# rebuild in worktree A after worktree B built into the same directory is a
# 0.2s no-op). This cut a fresh worktree's `make test` from ~5.3 to ~3
# minutes, with every later run incremental. Deliberately NOT the main
# checkout's target/, which rust-analyzer holds the build-directory lock on —
# the same contention ORCHESTRATOR_TARGET_DIR exists to avoid — and not a
# committed .cargo/config.toml, which would redirect the main checkout too.
# Two worktrees building at the same moment serialize on cargo's lock for the
# build phase only; the test run holds no lock. Nothing prunes the directory
# (the ORCHESTRATOR_TARGET_DIR precedent): expect ~4 GB warm, `rm -rf` is the
# reset. `?=`, so the real environment still outranks it. The HOME guard is
# the app-install one: with HOME unset the path would collapse to
# /.cache/..., and falling back to a per-worktree target/ costs only time.
ifneq (,$(findstring /.claude/worktrees/,$(CURDIR)))
ifneq (,$(wildcard $(HOME)))
CARGO_TARGET_DIR ?= $(HOME)/.cache/tasks-worktree-target
endif
endif
CARGO_TARGET_DIR ?= target
TEST_BIN_DIR := $(abspath $(CARGO_TARGET_DIR)/debug)

.PHONY: check-toolchain scout-supervisor-linux builder-supervisor-linux \
        vm-supervisor-linux image-base image-agent image-scout image-builder images \
        images-check \
        check-nextest test-bins test test-ci test-cargo app run \
        check-darwin app-build app-stop app-install \
        server-release dist-install dist \
        check-signing check-notary release-bundle sign notarize dmg \
        notarize-dmg verify-release check-clean-tree release release-clean \
        app-check app-stubs app-test \
        server serve restart status stop drain resume check-quiesced \
        migration verify-warm site-check

# Extra flags for the reload targets: `make restart RELOAD=--when-idle`.
RELOAD ?=
# ...and for `stop`, deliberately a separate variable: `stop` rejects --force
# and --no-build, so one shared variable would turn a typo into a usage error.
STOP ?=
# ...and for `drain`, for the same reason again: its flags are its own
# (--check, --cancel-scouts) and no other target accepts them.
DRAIN ?=
TASKS_BIN := $(CARGO_TARGET_DIR)/debug/tasks

# The build identity stamped into every artifact this Makefile installs:
# version is 0.1.<commit count>, commit is the short SHA, "-dirty" when
# uncommitted changes were present. Answers "is what I'm running fresh?" at a
# glance, and — since the server, the app and both supervisors compute it the
# same way — makes those numbers comparable to each other.
#
# Each build.rs computes the same two values on its own for a bare
# `cargo build`; passing them explicitly is what makes an *installed* artifact
# exact, and it is what lets `images-check` compare an image against the same
# expression that built it.
BUILD_VERSION := 0.1.$(shell git rev-list --count HEAD)
BUILD_COMMIT := $(shell git rev-parse --short HEAD)$(shell git diff --quiet 2>/dev/null || echo "-dirty")
# Aliases, so `make app` reads the way it always did. One pair of values, not
# two that could drift.
APP_VERSION := $(BUILD_VERSION)
APP_COMMIT := $(BUILD_COMMIT)

# Where the installed bundle lives. One source for a path that used to be
# written out twice — once by the installer, once by the launcher — which is
# how they could drift.
#
# `$$HOME` rather than `$(HOME)`, so the *shell* expands it inside the recipe —
# which is exactly what the two hand-written copies did, making this a rename
# rather than a change in behaviour.
#
# It is not, on its own, a guard against an unset HOME: `$$HOME` and `$(HOME)`
# both expand to the empty string there, and `"$HOME/Applications/Tasks.app"`
# then reads `/Applications/Tasks.app` — someone else's install, aimed at by an
# `rm -rf`. Measured, not assumed. The guard in `app-install` is what closes it.
#
# Named twice on purpose: `APP_BUNDLE` is what every recipe uses and what a
# command-line override replaces, and `APP_BUNDLE_DEFAULT` is what `app-install`
# compares against to ask "am I installing to the operator's own app?". The two
# guards that only make sense for that install — the HOME check and the "Tasks
# is running" note — key off that one comparison rather than off two conditions
# that could drift. `release-bundle` overrides `APP_BUNDLE` with an absolute
# path under `dist/`, where neither guard has anything to say: an absolute
# override cannot collapse to `/Applications/Tasks.app`, and the operator's
# running app is not the bundle being written.
APP_BUNDLE_DEFAULT := $$HOME/Applications/Tasks.app
APP_BUNDLE := $(APP_BUNDLE_DEFAULT)

# The macOS guard, named once. It used to sit in the recipe that both built
# and installed; those are separate targets now, so copying it would be two
# places to keep in step.
check-darwin:
	@[ "$$(uname -s)" = "Darwin" ] || { echo "the app targets build a macOS .app bundle; this is $$(uname -s)"; exit 1; }

# Build the mac app. app-gpui is not a workspace member and has its own
# target/ directory, so the binary comes from there, not the root target/.
app-build: check-darwin
	cd app-gpui && TASKS_GPUI_VERSION=$(APP_VERSION) TASKS_GPUI_COMMIT=$(APP_COMMIT) \
		cargo build --release

# Quit a running Tasks and *wait for it to be gone*.
#
# The waiting is the point. `pkill` returns as soon as the signal is delivered,
# not when the process has exited, so a bare `pkill; open` asks LaunchServices
# to activate an instance that is on its way out — which is the -600
# (procNotFound) that made `make run` fail on its first invocation and succeed
# on its second (#928).
#
# Bounded at 2s and it *falls through with a warning* rather than escalating to
# SIGKILL: SIGKILLing a window somebody is looking at is not this target's call,
# and a target that can hang is worse than one that can be wrong. It exits 0 in
# every case, including the warning — `exit 0` from inside the loop is what
# skips the warning, so there is no flag variable to get out of step.
#
# The kill and the wait use the same predicate (`-x Tasks`) deliberately: a
# wait watching a different process than the signal went to would report
# success for the wrong reason.
#
# No check-darwin. Quitting a process that isn't running is a no-op everywhere,
# and this is the one app target with nothing macOS-shaped in it.
app-stop:
	@pkill -x Tasks 2>/dev/null || true
	@for i in $$(seq 1 20); do \
		pgrep -x Tasks >/dev/null 2>&1 || exit 0; \
		sleep 0.1; \
	done; \
	echo "warning: Tasks still running after 2s; continuing (it may need to be quit by hand)"

# Assemble the bundle around the built binary and install it to ~/Applications,
# replacing any existing copy. There is no Xcode project any more, so this is
# done by hand.
#
# It warns rather than quitting a live app: `make app` is "build and install",
# not "restart my app", and killing the user's app from a build target is a
# surprise. `make run` is the target that stops it, and does so *before* this
# one runs.
#
# The HOME check is ahead of the `rm -rf`, not decorative: with HOME unset the
# path collapses to `/Applications/Tasks.app` and this recipe deletes whatever
# is there. Refusing costs a Mac user nothing — HOME is always set in a login
# shell — and it is the only line here that can destroy something.
#
# Contents/Resources/third-party is not decoration: the bundle links
# Apache-2.0 code (gpui-unofficial, and gpuikit's dual-licensed Apache arm),
# and §4(a) asks that a copy of the License travel with a binary
# distribution. The plist's NSHumanReadableCopyright points at it rather than
# claiming MIT for the whole artifact — which is #983's own defect, a license
# assertion with nothing behind it, one level out. Copied here rather than in
# `dist-install` because `make app` is a redistribution too the moment anyone
# hands the bundle over.
app-install: check-darwin
	@bundle="$(APP_BUNDLE)"; \
	if [ "$$bundle" = "$(APP_BUNDLE_DEFAULT)" ]; then \
		[ -n "$$HOME" ] || { echo "HOME is unset; refusing to install to $$bundle"; exit 1; }; \
		if pgrep -x Tasks >/dev/null 2>&1; then \
			echo "note: Tasks is running; it will keep running from the deleted bundle until you quit it (make run stops it first)"; \
		fi; \
	fi; \
	rm -rf "$$bundle"; \
	mkdir -p "$$bundle/Contents/MacOS" "$$bundle/Contents/Resources"; \
	cp app-gpui/target/release/tasks-gpui "$$bundle/Contents/MacOS/Tasks"; \
	sed -e 's/@VERSION@/$(APP_VERSION)/' -e 's/@COMMIT@/$(APP_COMMIT)/' \
		app-gpui/Info.plist.in > "$$bundle/Contents/Info.plist"; \
	cp -R app-gpui/third-party "$$bundle/Contents/Resources/third-party"; \
	echo "installed $$bundle ($(APP_VERSION), $(APP_COMMIT))"

# Build and install, exactly as `make app` always did.
#
# Sub-makes rather than prerequisites, here and in `run` below: `make -j` gives
# prerequisites no ordering at all, and every property of these targets is an
# ordering. Same call the `images-check` comment further down makes, for the
# same reason.
app:
	@$(MAKE) --no-print-directory app-build
	@$(MAKE) --no-print-directory app-install

# The self-contained bundle: `make app` plus a release `tasks` binary at
# Contents/Helpers/tasks — the distribution an end user downloads, and the
# stable home that survives a `cargo clean` on a dev machine (the serving
# binary used to live in target/, which is a build cache, not a home; see
# docs/plans/2026-08-18-end-user-distribution.md).
#
# Helpers, NOT Contents/MacOS: the app binary there is `Tasks`, and the
# default macOS filesystem is case-insensitive, so `MacOS/tasks` is the same
# directory entry — a cp "beside" the app binary silently overwrites it and
# every claim about the bundle stays green while the app is gone. Measured,
# not assumed: the first dist build did exactly that. Helpers is a standard
# nested-code location, so the signing chain below needs no exception for it
# (docs/plans/2026-08-19-signing-and-notarization.md — phase 2 is now here).
#
# `make app` stays the dev bundle — no embedded server, no release-build tax
# on the inner loop. The app treats the two identically except that a bundle
# carrying its own `tasks` is driven with `--no-build`, a decision the app
# derives from the binary's surroundings rather than from which target built
# the bundle (app-gpui/src/server.rs).
server-release:
	TASKS_SERVER_VERSION=$(BUILD_VERSION) TASKS_SERVER_COMMIT=$(BUILD_COMMIT) \
		cargo build --release -p tasks

# Embed the release server binary into an already-installed bundle. Separate
# from app-install so `make app` keeps meaning what it always meant, and a
# recipe rather than a cp in `dist`, so the failure ("no release binary") has
# a name and a fix instead of a bare cp error.
dist-install: check-darwin
	@[ -f "$(CARGO_TARGET_DIR)/release/tasks" ] || { \
		echo "no release tasks binary at $(CARGO_TARGET_DIR)/release/tasks; run make dist"; exit 1; }
	@bundle="$(APP_BUNDLE)"; \
	[ -d "$$bundle/Contents/MacOS" ] || { echo "no installed bundle at $$bundle; run make dist"; exit 1; }; \
	mkdir -p "$$bundle/Contents/Helpers"; \
	cp "$(CARGO_TARGET_DIR)/release/tasks" "$$bundle/Contents/Helpers/tasks"; \
	echo "embedded server binary in $$bundle ($(BUILD_VERSION), $(BUILD_COMMIT))"

# Sub-makes for the ordering, as everywhere else in this file. app-build runs
# first for the reason `run` gives: a compile error must cost nothing.
dist:
	@$(MAKE) --no-print-directory app-build
	@$(MAKE) --no-print-directory server-release
	@$(MAKE) --no-print-directory app-install
	@$(MAKE) --no-print-directory dist-install

# ---------------------------------------------------------------------------
# The release chain: a signed, notarized, stapled download.
#
# `make app` and `make dist` stay unsigned and unchanged. They never leave the
# machine that built them, which is exactly why they are fine and a download is
# not — Gatekeeper refuses an unsigned, un-notarized bundle that arrived with a
# quarantine attribute, and the failure lands on someone with no terminal in
# front of them.
#
# Every target here REFUSES rather than degrades. No signing identity is an
# error and never an unsigned artifact, because the failure worth preventing is
# the quiet one: the `.dmg` that built fine and reached a download link
# unsigned. The checklist, the pitfalls and the reasons live in
# docs/plans/2026-08-19-signing-and-notarization.md.
#
# Nothing in this block can run anywhere but macOS: codesign, xcrun, security,
# hdiutil, ditto and spctl are all macOS-only.
#
# Note the collision worth knowing about before you type either: `make dist`
# does NOT write to `dist/` — it installs to $(APP_BUNDLE_DEFAULT). `dist/` is
# this block's staging directory and `release-clean` empties only that.
DIST_DIR := $(abspath dist)
RELEASE_BUNDLE := $(DIST_DIR)/Tasks.app
DMG := $(DIST_DIR)/Tasks-$(BUILD_VERSION).dmg
CLI_ZIP := $(DIST_DIR)/tasks-$(BUILD_VERSION)-macos-arm64.zip

# Empty on purpose: there is no default that could be right, and a wrong
# default would sign with somebody else's certificate or fall through to an
# unsigned artifact. Pass it on the command line or export it.
SIGN_IDENTITY ?=
NOTARY_PROFILE ?= tasks-notary

# Split from check-notary because the two fail for different reasons and only
# `sign` and `dmg` need an identity — a notarization retry should not demand
# one, and a signing run should not demand App Store Connect credentials.
#
# It must be a *Developer ID Application* identity: the notary service rejects
# an Apple Development one by name, minutes later, which is exactly the slow
# way to learn it.
check-signing: check-darwin
	@[ -n "$(SIGN_IDENTITY)" ] || { \
		echo "SIGN_IDENTITY is unset; refusing to produce an unsigned release artifact."; \
		echo "  make release SIGN_IDENTITY=\"Developer ID Application: Your Name (TEAMID)\""; \
		echo "  security find-identity -v -p codesigning   # lists what this machine has"; \
		exit 1; }
	@security find-identity -v -p codesigning 2>/dev/null | grep -qF "$(SIGN_IDENTITY)" || { \
		echo "no codesigning identity matching \"$(SIGN_IDENTITY)\" in this keychain."; \
		echo "  security find-identity -v -p codesigning   # lists what this machine has"; \
		echo "It must be a 'Developer ID Application' identity — the notary service"; \
		echo "rejects an 'Apple Development' one by name."; \
		exit 1; }
	@echo "signing identity: $(SIGN_IDENTITY)"

# A liveness probe, not a presence check: `xcrun --find` only proves the tool
# is installed, and the credential is the half that is actually missing on a
# fresh machine. `history` is the cheapest call that needs one.
#
# The app-specific password never appears here, in argv, in `.env` or in the
# repo — `notarytool store-credentials` puts it in the keychain and everything
# downstream names the profile. Same rule the credential-custody work applies
# to every runtime secret; this one is a build-host secret and gets it anyway.
check-notary: check-darwin
	@xcrun --find notarytool >/dev/null 2>&1 || { \
		echo "no notarytool; install Xcode (or the Command Line Tools) and run:"; \
		echo "  sudo xcode-select --switch /Applications/Xcode.app"; \
		exit 1; }
	@xcrun notarytool history --keychain-profile "$(NOTARY_PROFILE)" >/dev/null 2>&1 || { \
		echo "notarytool cannot use keychain profile \"$(NOTARY_PROFILE)\"; store it once:"; \
		echo "  xcrun notarytool store-credentials \"$(NOTARY_PROFILE)\" \\"; \
		echo "    --apple-id <apple-id> --team-id <TEAMID> --password <app-specific-password>"; \
		echo "(the password is generated at appleid.apple.com and lives only in the keychain)"; \
		exit 1; }
	@echo "notary profile: $(NOTARY_PROFILE)"

# The release bundle is assembled by the SAME two recipes as the dev bundle,
# with APP_BUNDLE overridden on the sub-make command line — a command-line
# variable beats the `:=` above and propagates to sub-makes, so nothing else
# needs to change. A second copy of app-install/dist-install aimed at dist/ is
# how the two bundles would come to differ in a way nobody noticed until a
# download behaved unlike the thing that was tested.
#
# app-gpui is not a workspace member and has its own target/, so the app binary
# and the server binary come from two different build directories; this target
# is what drives both.
release-bundle: check-darwin
	@$(MAKE) --no-print-directory app-build
	@$(MAKE) --no-print-directory server-release
	@$(MAKE) --no-print-directory app-install APP_BUNDLE=$(RELEASE_BUNDLE)
	@$(MAKE) --no-print-directory dist-install APP_BUNDLE=$(RELEASE_BUNDLE)

# Sign inside-out, and NEVER --deep. `--deep` re-signs nested code with the
# outer bundle's arguments; the documented failure is a signature that verifies
# locally and comes back Invalid from the notary service minutes later.
#
# --force because Rust binaries arrive ad-hoc signed (the linker does it on
# Apple Silicon), so codesign would otherwise fail on an already-signed binary
# rather than doing nothing.
#
# The standalone CLI is a *copy of the signed seed*, not a second signing act,
# so the binary in the release and the binary inside the app are byte
# identical. A Mach-O signature lives in the file, so it survives the copy —
# and survives `tasks service install`'s copy to ~/.tasks/bin/tasks too.
#
# The three local checks at the end are the whole reason this target verifies
# anything: a genuine signing defect comes back from the notary service as
# Invalid, but a submission can also sit In Progress for hours, and telling
# those apart after the fact is expensive. Locally the answer takes a second.
#
# No entitlements file, deliberately. Hardened runtime needs no exception for a
# Metal app that spawns child processes and speaks HTTP, and an entitlements
# plist that grants nothing is a file that accretes grants. Add one only when a
# real hardened-runtime failure names the exception it needs — and never
# `--options` without `runtime` to make a crash go away.
sign: check-darwin check-signing
	@[ -d "$(RELEASE_BUNDLE)" ] || { \
		echo "no bundle at $(RELEASE_BUNDLE); run make release-bundle"; exit 1; }
	@[ -f "$(RELEASE_BUNDLE)/Contents/Helpers/tasks" ] || { \
		echo "no seed binary at $(RELEASE_BUNDLE)/Contents/Helpers/tasks; run make release-bundle"; exit 1; }
	codesign --force --options runtime --timestamp \
		--sign "$(SIGN_IDENTITY)" "$(RELEASE_BUNDLE)/Contents/Helpers/tasks"
	codesign --force --options runtime --timestamp \
		--sign "$(SIGN_IDENTITY)" "$(RELEASE_BUNDLE)"
	cp "$(RELEASE_BUNDLE)/Contents/Helpers/tasks" "$(DIST_DIR)/tasks"
	codesign --verify --strict --verbose=2 "$(RELEASE_BUNDLE)"
	codesign --verify --strict --verbose=2 "$(DIST_DIR)/tasks"
	@for target in "$(RELEASE_BUNDLE)" "$(DIST_DIR)/tasks"; do \
		if codesign -d --entitlements :- "$$target" 2>/dev/null | grep -q 'get-task-allow'; then \
			echo "$$target carries get-task-allow: a debug entitlement the notary service rejects"; \
			exit 1; \
		fi; \
		codesign -d -vv "$$target" 2>&1 | grep -q 'flags=.*runtime' || { \
			echo "$$target is not signed with the hardened runtime (--options runtime)"; \
			exit 1; }; \
	done
	@echo "signed $(RELEASE_BUNDLE) and $(DIST_DIR)/tasks (hardened runtime, timestamped)"

# ONE submission carrying both artifacts, so the app and the standalone CLI
# cannot end up notarized against different signatures.
#
# `stapler` is the gate, not `--wait`: a ticket exists only for an Accepted
# submission, so a staple that succeeds is the proof, and a staple that fails
# is where a rejection stops the chain.
#
# A bare Mach-O CANNOT be stapled — Apple staples bundles, disk images and flat
# installer packages only, and against a raw executable stapler fails looking
# for Contents/CodeResources. That is printed rather than left in a doc,
# because whoever runs this is the person who will otherwise try it.
notarize: check-darwin check-notary
	@[ -d "$(RELEASE_BUNDLE)" ] || { echo "no bundle at $(RELEASE_BUNDLE); run make sign"; exit 1; }
	@[ -f "$(DIST_DIR)/tasks" ] || { echo "no signed CLI at $(DIST_DIR)/tasks; run make sign"; exit 1; }
	rm -rf "$(DIST_DIR)/submission" "$(DIST_DIR)/submission.zip"
	mkdir -p "$(DIST_DIR)/submission"
	ditto "$(RELEASE_BUNDLE)" "$(DIST_DIR)/submission/Tasks.app"
	ditto "$(DIST_DIR)/tasks" "$(DIST_DIR)/submission/tasks"
	ditto -c -k --keepParent "$(DIST_DIR)/submission" "$(DIST_DIR)/submission.zip"
	xcrun notarytool submit "$(DIST_DIR)/submission.zip" \
		--keychain-profile "$(NOTARY_PROFILE)" --wait
	xcrun stapler staple "$(RELEASE_BUNDLE)"
	ditto -c -k "$(DIST_DIR)/tasks" "$(CLI_ZIP)"
	@echo "stapled $(RELEASE_BUNDLE); wrote $(CLI_ZIP)"
	@echo "note: $(CLI_ZIP) is notarized but NOT stapled — a bare Mach-O cannot be."
	@echo "      Gatekeeper fetches its ticket online at first launch of a quarantined copy."

# Refuses unless the bundle is already stapled. That precondition is what stops
# the silent version of the ordering mistake: a DMG built before notarization
# looks identical and ships an app whose first launch needs Apple reachable.
#
# The /Applications symlink is not decoration — it makes the drag the obvious
# gesture, and dragging out of the image is what clears App Translocation (a
# quarantined app launched from the mounted image runs from a randomized
# read-only path).
#
# The image is signed WITHOUT --options runtime: a disk image is not executable
# code, and hardened runtime is a property of a running process.
dmg: check-darwin check-signing
	@xcrun stapler validate "$(RELEASE_BUNDLE)" >/dev/null 2>&1 || { \
		echo "$(RELEASE_BUNDLE) is not stapled; run make notarize first."; \
		echo "A DMG built before notarization looks identical and ships an app whose"; \
		echo "first launch needs Apple reachable."; \
		exit 1; }
	rm -rf "$(DIST_DIR)/dmg" "$(DMG)"
	mkdir -p "$(DIST_DIR)/dmg"
	ditto "$(RELEASE_BUNDLE)" "$(DIST_DIR)/dmg/Tasks.app"
	ln -s /Applications "$(DIST_DIR)/dmg/Applications"
	hdiutil create -format UDZO -volname "Tasks" -srcfolder "$(DIST_DIR)/dmg" -ov "$(DMG)"
	codesign --force --timestamp --sign "$(SIGN_IDENTITY)" "$(DMG)"
	@echo "wrote $(DMG)"

notarize-dmg: check-darwin check-notary
	@[ -f "$(DMG)" ] || { echo "no image at $(DMG); run make dmg"; exit 1; }
	xcrun notarytool submit "$(DMG)" --keychain-profile "$(NOTARY_PROFILE)" --wait
	xcrun stapler staple "$(DMG)"
	@echo "stapled $(DMG)"

# spctl is the one that answers the question actually being asked — what
# Gatekeeper will do — rather than "is this signature well formed". Both are
# here because they fail differently and only one of them is about Apple's
# verdict.
verify-release: check-darwin
	codesign --verify --strict --verbose=2 "$(RELEASE_BUNDLE)"
	codesign --verify --strict --verbose=2 "$(RELEASE_BUNDLE)/Contents/Helpers/tasks"
	xcrun stapler validate "$(RELEASE_BUNDLE)"
	xcrun stapler validate "$(DMG)"
	spctl -a -t exec -vv "$(RELEASE_BUNDLE)"
	spctl -a -t open --context context:primary-signature -vv "$(DMG)"
	@echo "verified $(RELEASE_BUNDLE) and $(DMG)"

# A release names a commit — #997 tags them v0.1.<commit count>, the same
# number BUILD_VERSION already carries — so a dirty tree makes the tag a lie.
# FORCE=1 overrides, matching the `make images` convention.
check-clean-tree:
	@if [ -n "$(FORCE)" ]; then \
		echo "FORCE=$(FORCE): releasing a dirty tree; $(BUILD_VERSION) will not name what shipped"; \
	else \
		git diff --quiet && git diff --cached --quiet || { \
			echo "uncommitted changes; a release names a commit and this tree is not one."; \
			echo "  git status        # then commit, stash, or FORCE=1 make release"; \
			exit 1; }; \
	fi

# The whole chain. Both credential checks and the tree check run FIRST, so a
# missing credential costs a message rather than a six-minute build.
#
# Sub-makes rather than prerequisites, as everywhere else in this file: `make
# -j` gives prerequisites no ordering at all, and every property of this
# sequence is an ordering.
release:
	@$(MAKE) --no-print-directory check-signing
	@$(MAKE) --no-print-directory check-notary
	@$(MAKE) --no-print-directory check-clean-tree
	@$(MAKE) --no-print-directory release-bundle
	@$(MAKE) --no-print-directory sign
	@$(MAKE) --no-print-directory notarize
	@$(MAKE) --no-print-directory dmg
	@$(MAKE) --no-print-directory notarize-dmg
	@$(MAKE) --no-print-directory verify-release

# No check-darwin: removing a directory works everywhere, and a clean that
# refuses to run on the wrong OS is a clean nobody can use to tidy up after a
# failed experiment. It empties `dist/` only — `make dist` writes to
# $(APP_BUNDLE_DEFAULT) and is untouched by this.
release-clean:
	rm -rf $(DIST_DIR)
# ---------------------------------------------------------------------------

# Build, stop, install, launch — in that order, and each step is where it is
# for a reason:
#
#   app-build    first, so a compile error costs you nothing: the app you have
#                running keeps running, exactly as a failed `make restart`
#                leaves the server you have serving.
#   app-stop     before the install, because app-install deletes the bundle and
#                a live process running out of a deleted bundle is what made
#                `open` fail with -600.
#   app-install  now safe: nothing is running out of the bundle it replaces.
#   open         the relaunch, retried once. Not `|| true` — `make run`
#                reporting success while nothing launched would be a worse bug
#                than the one this fixes. With the wait above in place the
#                retry should be dead code; it costs a line, and being wrong
#                about that costs a red build for a cosmetic race.
run:
	@$(MAKE) --no-print-directory app-build
	@$(MAKE) --no-print-directory app-stop
	@$(MAKE) --no-print-directory app-install
	@open "$(APP_BUNDLE)" || { \
		echo "launch failed; retrying once"; \
		sleep 1; \
		open "$(APP_BUNDLE)"; \
	}

# Typecheck and test the GUI on a machine with no display and no Mac — which
# is every agent VM, and used to mean every app-gpui change was written
# without a compile, let alone a test.
#
# With app-gpui's build dependencies installed (`pkg-config libfontconfig-dev
# libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev`, which the scout and
# builder images now carry) these are plain cargo commands, and so is a
# hand-run `cd app-gpui && cargo test`. Without them, two obstacles:
#
#   * `yeslogic-fontconfig-sys`'s build.rs calls `pkg_config::find_library`
#     and fails — but the same build.rs skips pkg-config entirely when
#     RUST_FONTCONFIG_DLOPEN is set, which is all `app-check` needs.
#   * linking the *test* binary additionally wants -lxcb and -lxkbcommon(-x11).
#     Nothing in these tests calls them: they are pure functions over view
#     state and never enter the platform layer. Empty stub .so's satisfy the
#     linker, and a test that did reach the platform would fail loudly rather
#     than quietly pass.
#
# The workaround has to stay — the image change is inert until someone runs
# `make images`, so every VM alive before that still needs it — but it must
# not stay unconditional. `-L $(APP_STUB_DIR)` is searched before the system
# paths, so the empty stubs shadow the real libraries wherever both exist, and
# RUSTFLAGS is part of cargo's fingerprint, so a stubbed `make app-test` and a
# hand-run `cargo test` each rebuild the whole gpui tree over the other.
#
# pkg-config is itself one of the five packages, so its absence is the same
# answer as a missing header, and macOS (where none of this is needed and
# `cd app-gpui && cargo test` is the shorter path) reaches the plain branch
# via Homebrew's pkg-config or the fallback, both of which work.
#
# The app itself still comes from `make app` on a Mac; this proves the code
# compiles and its logic holds, not that a pixel landed anywhere.
APP_STUB_DIR := $(abspath $(CARGO_TARGET_DIR)/app-link-stubs)
APP_DEPS_INSTALLED := $(shell pkg-config --exists fontconfig xkbcommon xkbcommon-x11 xcb 2>/dev/null && echo yes)

ifeq ($(APP_DEPS_INSTALLED),yes)
APP_CHECK_ENV :=
APP_TEST_ENV :=
APP_TEST_PREREQS :=
else
APP_CHECK_ENV := RUST_FONTCONFIG_DLOPEN=1
APP_TEST_ENV := RUST_FONTCONFIG_DLOPEN=1 RUSTFLAGS="-L $(APP_STUB_DIR)"
APP_TEST_PREREQS := app-stubs
endif

app-check:
	cd app-gpui && $(APP_CHECK_ENV) cargo check --all-targets

app-stubs:
	@mkdir -p $(APP_STUB_DIR)
	@printf 'void tasks_gpui_link_stub(void) {}\n' > $(APP_STUB_DIR)/stub.c
	@for lib in xcb xkbcommon xkbcommon-x11; do \
		cc -shared -fPIC -o $(APP_STUB_DIR)/lib$$lib.so $(APP_STUB_DIR)/stub.c || exit 1; \
	done

app-test: $(APP_TEST_PREREQS)
	cd app-gpui && $(APP_TEST_ENV) cargo test

# The server's own build/run loop, the same shape as `make run` for the app:
# these swap a running server rather than refusing. Every target builds first,
# then signals — a failed build must never cost you the server you have. The
# freshly built binary is the one that does the swapping (`--no-build`), so
# make owns the build and `tasks reload` owns the handover.
#
#   make serve                    build, take over, log to this terminal
#   make restart                  build, take over, background it
#   make restart RELOAD=--when-idle   ... but wait out in-flight scouts first
#   make stop STOP=--when-idle    stop, but wait out in-flight scouts first
server:
	cargo build -p tasks

serve: server
	$(TASKS_BIN) reload --no-build --foreground $(RELOAD)

restart: server
	$(TASKS_BIN) reload --no-build $(RELOAD)

# `tasks status` exits 1 when nothing is serving, which is the right contract
# for a script and pure noise here ("make: *** Error 1" on a correct answer).
# Scripts should call the binary, not make.
status: server
	@$(TASKS_BIN) status || true

# `make stop STOP=--when-idle` waits out in-flight scouts and builds first,
# on the same predicate `restart RELOAD=--when-idle` waits on. It leaves
# dispatch paused, because nothing follows it that could carry the mode.
stop: server
	@$(TASKS_BIN) stop $(STOP)

# Quiesce the pipeline for host work this repo's own tooling has to do to the
# machine rather than to the server: restarting vm-pool (the successor stops
# its predecessor's containers off the orphan ledger) and `make images`.
# Neither is something `tasks reload` covers, because a reload re-attaches to
# every live VM and these do not.
#
# `make drain DRAIN=--cancel-scouts` stops running scouts instead of waiting
# them out. The hold outlives the command: `make resume` is what gives it back.
drain: server
	@$(TASKS_BIN) drain $(DRAIN)

resume: server
	@$(TASKS_BIN) resume

# The gate `make images` runs before it rebuilds anything, and the reason it
# is not merely advisory: a scout dispatched while the rebuild is in flight
# starts in the OLD image — the #909 staleness the update hold exists to
# prevent, and the one case it cannot see, since the identity it reads is only
# ever observed from a run that has already started.
#
# It passes with nothing serving (no dispatcher, nothing that can start a
# container), and refuses a *playing* pipeline even with nothing in flight,
# because the dispatcher tops scouts up on its next tick. FORCE=1 is the
# escape hatch for someone who knows better.
check-quiesced: server
	@if [ -n "$(FORCE)" ]; then \
		echo "FORCE=$(FORCE): skipping the drain check"; \
	else \
		$(TASKS_BIN) drain --check; \
	fi

# Prime the orchestrator's verification build directory, so the first merge
# decision it makes is not also the first cold build.
#
# Path resolution mirrors `Config::orchestrator_target_dir`: ORCHESTRATOR_TARGET_DIR,
# else <data dir>/verify-target, where the data dir is TASKS_DATA_DIR else
# ~/.local/state/tasks-v2. `$(if $(strip ...))` rather than `?=`, because `?=`
# treats an exported-but-empty variable as set while the server's `env_string`
# filters empty out — the two would then disagree and this would warm a
# directory nothing uses.
#
# CARGO_TARGET_DIR is set INLINE on the one command, never as a make-level
# export: an exported one would redirect TEST_BIN_DIR (derived from it at the
# top of this file) and the suites would look for binaries nothing built.
VERIFY_DATA_DIR := $(if $(strip $(TASKS_DATA_DIR)),$(TASKS_DATA_DIR),$(HOME)/.local/state/tasks-v2)
VERIFY_TARGET_DIR := $(if $(strip $(ORCHESTRATOR_TARGET_DIR)),$(ORCHESTRATOR_TARGET_DIR),$(VERIFY_DATA_DIR)/verify-target)

# The three cargo settings beside it are `orchestrator::VERIFICATION_ENV`, and
# they must be set HERE AND ON THE CHILD OR NEITHER: toggling either one
# invalidates every workspace artifact, so a Makefile and a server that
# disagreed would rebuild the whole workspace on every alternation between them
# — costing far more than the disk they save. `verification_env_matches_the_makefile`
# fails the suite if these drift apart. Measured 2026-08-20: this build is
# 6.26 GB at the default debuginfo and 3.16 GB at line-tables-only, which still
# names a file and a line in every backtrace frame.
verify-warm:
	@echo "warming $(VERIFY_TARGET_DIR) (bounded by ORCHESTRATOR_TARGET_BUDGET_GB, default 20 GB; size is on \`tasks status\`)"
	@mkdir -p $(VERIFY_TARGET_DIR)
	CARGO_TARGET_DIR=$(VERIFY_TARGET_DIR) \
		CARGO_INCREMENTAL=0 \
		CARGO_PROFILE_DEV_DEBUG=line-tables-only \
		CARGO_PROFILE_TEST_DEBUG=line-tables-only \
		cargo test --workspace --no-run

# A new migration, named for this UTC instant:
#
#   make migration NAME=build_transcripts
#   -> crates/tasks/migrations/20260815030411_build_transcripts.sql
#
# Never hand-roll one by copying the file next to it and adding a number: the
# "next free number" is read off a tree that cannot see its sibling branches,
# so two of them pick the same one and the collision surfaces after the merge,
# at boot. The rule and the tests that enforce it are in
# crates/tasks/src/migrations.rs; digits only, because sqlx parses the part
# before the first `_` as an i64.
#
# NAME becomes sqlx's description, and is what the guard test reconstructs the
# filename from, so it is checked here rather than left to fail later.
migration:
	@[ -n "$(NAME)" ] || { echo "usage: make migration NAME=lower_snake_case"; exit 1; }
	@echo "$(NAME)" | grep -Eq '^[a-z][a-z0-9]*(_[a-z0-9]+)*$$' || { \
		echo "NAME must be lower_snake_case: $(NAME)"; exit 1; }
	@file="crates/tasks/migrations/$$(date -u +%Y%m%d%H%M%S)_$(NAME).sql"; \
	if [ -e "$$file" ]; then echo "already exists: $$file"; exit 1; fi; \
	echo "-- $(NAME)" > "$$file"; \
	echo "$$file"

# Tests. Binaries are built once, here, and the suite only execs them —
# nothing shells out to `cargo build` mid-test (that used to block on the
# build-directory lock, so anything else touching it stalled the run).
#
# nextest does not run doctests at all, and does not say so in its summary,
# so both `test` and `test-ci` finish with an explicit doctest pass.

check-nextest:
	@command -v cargo-nextest >/dev/null || { \
		echo "missing cargo-nextest: cargo install cargo-nextest --locked"; \
		echo "(or run 'make test-cargo' to use plain cargo test)"; exit 1; }

# Prebuild the binaries the suites exec, so TASKS_TEST_BIN_DIR and
# VM_POOL_TEST_BIN_DIR are populated before the first test process starts.
# vm-pool has its own variable on purpose: it is vendored infrastructure and
# nothing from the surrounding tasks workspace may leak into it.
test-bins:
	cargo build -p scout-supervisor -p builder-supervisor -p vm-pool-supervisor

# Both variables name the same directory today; they are separate names, not
# one shared name, so vm-pool keeps working when it is extracted.
TEST_BIN_ENV := TASKS_TEST_BIN_DIR=$(TEST_BIN_DIR) VM_POOL_TEST_BIN_DIR=$(TEST_BIN_DIR)

test: check-nextest test-bins
	$(TEST_BIN_ENV) cargo nextest run --workspace
	cargo test --doc --workspace

test-ci: check-nextest test-bins
	$(TEST_BIN_ENV) cargo nextest run --workspace --profile ci
	cargo test --doc --workspace

# The no-prerequisites path. Deliberately exports neither bin-dir variable,
# so the build-on-demand fallback stays exercised.
test-cargo:
	cargo test --workspace

# The whole publish gate for `site/`: there is no build step, so this script is
# all that stands between a bad edit and a deployed page. The Pages workflow
# runs the same line. Missing screenshots are errors, not warnings — see
# `site/img/MANIFEST.md`.
site-check:
	@bash site/check.sh

check-toolchain:
	@which $(CROSS_LINKER) >/dev/null || { echo "missing cross linker: brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu"; exit 1; }
	@rustup target list --installed | grep -q $(LINUX_TARGET) || { echo "missing rust target: rustup target add $(LINUX_TARGET)"; exit 1; }
	@which container >/dev/null || { echo "missing apple/container CLI"; exit 1; }

# Both supervisors are stamped explicitly, for the same reason `make app` is:
# an installed artifact should carry the identity of the tree that produced it,
# not of whatever the build container could see — and `images-check` compares
# what the image reports against these exact values.
scout-supervisor-linux: check-toolchain
	$(CROSS_ENV) SCOUT_SUPERVISOR_VERSION=$(BUILD_VERSION) SCOUT_SUPERVISOR_COMMIT=$(BUILD_COMMIT) \
		cargo build --release --target $(LINUX_TARGET) -p scout-supervisor

builder-supervisor-linux: check-toolchain
	$(CROSS_ENV) BUILDER_SUPERVISOR_VERSION=$(BUILD_VERSION) BUILDER_SUPERVISOR_COMMIT=$(BUILD_COMMIT) \
		cargo build --release --target $(LINUX_TARGET) -p builder-supervisor

vm-supervisor-linux: check-toolchain
	$(CROSS_ENV) cargo build --release --target $(LINUX_TARGET) -p vm-pool-supervisor

image-base: vm-supervisor-linux
	cp $(VM_SUPERVISOR_BIN) images/base/supervisor
	container build -t vm-pool-base:latest images/base
	rm -f images/base/supervisor

image-agent: image-base
	container build -t vm-pool-agent:latest images/agent

# The Scout image Tasks dispatches (SCOUT_IMAGE default).
image-scout: image-agent scout-supervisor-linux
	cp $(SCOUT_BIN) images/scout/scout-supervisor
	container build -t agent:v1 images/scout
	rm -f images/scout/scout-supervisor

# The Builder image (BUILDER_IMAGE default).
image-builder: image-agent builder-supervisor-linux
	cp $(BUILDER_BIN) images/builder/builder-supervisor
	container build -t builder:v1 images/builder
	rm -f images/builder/builder-supervisor

# Sub-makes rather than prerequisites, for the reason `app`, `run` and
# `images-check` already give: `make -j` gives prerequisites no ordering at
# all, and every property of this sequence is an ordering. The gate has to run
# *before* the build, or it is checking the state the rebuild already raced.
images:
	@$(MAKE) --no-print-directory check-quiesced
	@$(MAKE) --no-print-directory image-scout
	@$(MAKE) --no-print-directory image-builder
	@$(MAKE) --no-print-directory images-check

# Boot each image and read `--version` out of it.
#
# This covers the one window the run-time observation cannot: right after a
# rebuild, before anything has run. Nothing polls an image — a VM exists only
# while a run is inside it — so until a scout or a build starts, the server has
# no reading at all.
#
# A recipe line of `images` rather than a third prerequisite: `make -j` gives
# prerequisites no ordering, and a check that can race the build it verifies is
# worse than no check.
#
# The probe is deliberately not wrapped in `timeout(1)` — macOS ships none, and
# it does not need one: `container run` without `-i` hands the supervisor a
# closed stdin, so a pre-stamping supervisor falls through argv into its
# JSON-lines loop, reads EOF, and exits. The shape check below then reports
# that as "too old to report a version", which is the honest answer.
images-check:
	@which container >/dev/null || { echo "missing apple/container CLI"; exit 1; }
	@case "$(BUILD_COMMIT)" in *-dirty) \
		echo "warning: this tree is dirty ($(BUILD_COMMIT)); an image built from it stamps"; \
		echo "         the same -dirty string and compares equal while naming a commit it"; \
		echo "         may not contain. The comparison below is not exact.";; \
	esac
	@stale=0; \
	for pair in "agent:v1 scout-supervisor" "builder:v1 builder-supervisor"; do \
		set -- $$pair; image="$$1"; name="$$2"; \
		out="$$(container run --rm "$$image" --version 2>/dev/null | head -n 1)"; \
		set -- $$out; \
		if [ "$$1" != "$$name" ] || [ -z "$$3" ]; then \
			echo "$$image  too old to report a version (predates supervisor stamping) — run make images"; \
			stale=1; continue; \
		fi; \
		if [ "$$2" = "$(BUILD_VERSION)" ] && [ "$$3" = "$(BUILD_COMMIT)" ]; then \
			echo "$$image  $$2 ($$3)  current"; \
		else \
			echo "$$image  $$2 ($$3)  stale — this tree is $(BUILD_VERSION) ($(BUILD_COMMIT)) — run make images"; \
			stale=1; \
		fi; \
	done; \
	exit $$stale
