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
CARGO_TARGET_DIR ?= target
TEST_BIN_DIR := $(abspath $(CARGO_TARGET_DIR)/debug)

.PHONY: check-toolchain scout-supervisor-linux builder-supervisor-linux \
        vm-supervisor-linux image-base image-agent image-scout image-builder images \
        images-check \
        check-nextest test-bins test test-ci test-cargo app run \
        app-check app-stubs app-test \
        server serve restart status stop migration verify-warm

# Extra flags for the reload targets: `make restart RELOAD=--when-idle`.
RELOAD ?=
# ...and for `stop`, deliberately a separate variable: `stop` rejects --force
# and --no-build, so one shared variable would turn a typo into a usage error.
STOP ?=
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

# Build the mac app and install it to ~/Applications, replacing any existing
# copy. app-gpui is not a workspace member and has its own target/ directory,
# so the binary comes from there, not the root target/. There is no Xcode
# project any more, so the bundle is assembled by hand around it.
app:
	@[ "$$(uname -s)" = "Darwin" ] || { echo "make app builds a macOS .app bundle; this is $$(uname -s)"; exit 1; }
	cd app-gpui && TASKS_GPUI_VERSION=$(APP_VERSION) TASKS_GPUI_COMMIT=$(APP_COMMIT) \
		cargo build --release
	@bundle="$$HOME/Applications/Tasks.app"; \
	rm -rf "$$bundle"; \
	mkdir -p "$$bundle/Contents/MacOS" "$$bundle/Contents/Resources"; \
	cp app-gpui/target/release/tasks-gpui "$$bundle/Contents/MacOS/Tasks"; \
	sed -e 's/@VERSION@/$(APP_VERSION)/' -e 's/@COMMIT@/$(APP_COMMIT)/' \
		app-gpui/Info.plist.in > "$$bundle/Contents/Info.plist"; \
	echo "installed $$bundle ($(APP_VERSION), $(APP_COMMIT))"

# Build, install, and (re)launch.
run: app
	@pkill -x Tasks 2>/dev/null || true
	open ~/Applications/Tasks.app

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

verify-warm:
	@echo "warming $(VERIFY_TARGET_DIR) (expect ~7.5 GB once built; nothing prunes it, by design)"
	@mkdir -p $(VERIFY_TARGET_DIR)
	CARGO_TARGET_DIR=$(VERIFY_TARGET_DIR) cargo test --workspace --no-run

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

images: image-scout image-builder
	@$(MAKE) images-check

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
