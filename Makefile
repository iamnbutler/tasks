# Container image pipeline. Prereqs (make check-toolchain):
#   brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu
#   rustup target add aarch64-unknown-linux-gnu
#   apple/container CLI (`container system start` before building images)

LINUX_TARGET := aarch64-unknown-linux-gnu
SCOUT_BIN := target/$(LINUX_TARGET)/release/scout-supervisor
BUILDER_BIN := target/$(LINUX_TARGET)/release/builder-supervisor
VM_SUPERVISOR_BIN := target/$(LINUX_TARGET)/release/supervisor

# Where cargo puts debug binaries, honouring an environment override.
# abspath because tests run with cwd set to their own package.
CARGO_TARGET_DIR ?= target
TEST_BIN_DIR := $(abspath $(CARGO_TARGET_DIR)/debug)

.PHONY: check-toolchain scout-supervisor-linux builder-supervisor-linux \
        vm-supervisor-linux image-base image-agent image-scout image-builder images \
        app run check-nextest test-bins test test-ci test-cargo

# Version identity stamped into the app (shown in About Tasks): version is
# 0.1.<commit count>, build is the short SHA, "-dirty" when uncommitted
# changes were present. Answers "is what I'm running fresh?" at a glance.
APP_VERSION := 0.1.$(shell git rev-list --count HEAD)
APP_COMMIT := $(shell git rev-parse --short HEAD)$(shell git diff --quiet 2>/dev/null || echo "-dirty")

# Build the mac app and install it to ~/Applications, replacing any existing
# copy. Uses xcodebuild's own answer for where the product landed, so this
# shares DerivedData (and its build cache) with Xcode.
app:
	xcodebuild -project app/Tasks.xcodeproj -scheme Tasks -configuration Debug -quiet \
		MARKETING_VERSION=$(APP_VERSION) CURRENT_PROJECT_VERSION=$(APP_COMMIT) build
	@built="$$(xcodebuild -project app/Tasks.xcodeproj -scheme Tasks -configuration Debug -showBuildSettings 2>/dev/null | awk '/ BUILT_PRODUCTS_DIR =/{print $$3; exit}')"; \
	mkdir -p ~/Applications; \
	rm -rf ~/Applications/Tasks.app; \
	ditto "$$built/Tasks.app" ~/Applications/Tasks.app; \
	echo "installed ~/Applications/Tasks.app"

# Build, install, and (re)launch.
run: app
	@pkill -x Tasks 2>/dev/null || true
	open ~/Applications/Tasks.app

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

# Prebuild the binaries the tasks suite execs, so TASKS_TEST_BIN_DIR is
# populated before the first test process starts.
test-bins:
	cargo build -p scout-supervisor -p builder-supervisor

test: check-nextest test-bins
	TASKS_TEST_BIN_DIR=$(TEST_BIN_DIR) cargo nextest run --workspace
	cargo test --doc --workspace

test-ci: check-nextest test-bins
	TASKS_TEST_BIN_DIR=$(TEST_BIN_DIR) cargo nextest run --workspace --profile ci
	cargo test --doc --workspace

# The no-prerequisites path. Deliberately does not export TASKS_TEST_BIN_DIR,
# so the build-on-demand fallback stays exercised.
test-cargo:
	cargo test --workspace

check-toolchain:
	@which $(LINUX_TARGET)-gcc >/dev/null || { echo "missing cross linker: brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu"; exit 1; }
	@rustup target list --installed | grep -q $(LINUX_TARGET) || { echo "missing rust target: rustup target add $(LINUX_TARGET)"; exit 1; }
	@which container >/dev/null || { echo "missing apple/container CLI"; exit 1; }

scout-supervisor-linux: check-toolchain
	cargo build --release --target $(LINUX_TARGET) -p scout-supervisor

builder-supervisor-linux: check-toolchain
	cargo build --release --target $(LINUX_TARGET) -p builder-supervisor

vm-supervisor-linux: check-toolchain
	cargo build --release --target $(LINUX_TARGET) -p vm-pool-supervisor

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
