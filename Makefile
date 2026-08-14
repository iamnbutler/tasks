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

.PHONY: check-toolchain scout-supervisor-linux builder-supervisor-linux \
        vm-supervisor-linux image-base image-agent image-scout image-builder images

check-toolchain:
	@which $(CROSS_LINKER) >/dev/null || { echo "missing cross linker: brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu"; exit 1; }
	@rustup target list --installed | grep -q $(LINUX_TARGET) || { echo "missing rust target: rustup target add $(LINUX_TARGET)"; exit 1; }
	@which container >/dev/null || { echo "missing apple/container CLI"; exit 1; }

scout-supervisor-linux: check-toolchain
	$(CROSS_ENV) cargo build --release --target $(LINUX_TARGET) -p scout-supervisor

builder-supervisor-linux: check-toolchain
	$(CROSS_ENV) cargo build --release --target $(LINUX_TARGET) -p builder-supervisor

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
