# Container image pipeline. Prereqs (make check-toolchain):
#   brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu
#   rustup target add aarch64-unknown-linux-gnu
#   apple/container CLI (`container system start` before building images)

LINUX_TARGET := aarch64-unknown-linux-gnu
SCOUT_BIN := target/$(LINUX_TARGET)/release/scout-supervisor
VM_SUPERVISOR_BIN := target/$(LINUX_TARGET)/release/supervisor

.PHONY: check-toolchain scout-supervisor-linux vm-supervisor-linux \
        image-base image-agent image-scout images

check-toolchain:
	@which $(LINUX_TARGET)-gcc >/dev/null || { echo "missing cross linker: brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu"; exit 1; }
	@rustup target list --installed | grep -q $(LINUX_TARGET) || { echo "missing rust target: rustup target add $(LINUX_TARGET)"; exit 1; }
	@which container >/dev/null || { echo "missing apple/container CLI"; exit 1; }

scout-supervisor-linux: check-toolchain
	cargo build --release --target $(LINUX_TARGET) -p scout-supervisor

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

images: image-scout
