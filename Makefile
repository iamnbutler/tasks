# Cross-compilation and container image build targets

# Target triple for Linux ARM64 (container runtime)
TARGET := aarch64-unknown-linux-gnu

# Cross-linker binary name (installed via brew on macOS or apt on Linux)
LINKER := $(TARGET)-gcc

# Output paths
SUPERVISOR_BIN := target/$(TARGET)/release/tasks-supervisor
CONTAINER_IMAGE := tasks-agent:latest

.PHONY: all run web check-linker supervisor container-image clean

all: container-image

# Build the web frontend and start the server.
# Exit code 100 means "update available" — pull, rebuild, and restart.
run: web
	@while true; do \
		cargo run -- run --web; \
		EXIT_CODE=$$?; \
		if [ $$EXIT_CODE -eq 100 ]; then \
			echo "Update requested (exit 100), pulling and rebuilding..."; \
			git pull --ff-only && \
			cd web && bun install && bun run build && cd .. && \
			echo "Rebuilt. Restarting..."; \
		else \
			exit $$EXIT_CODE; \
		fi; \
	done

# Build the web frontend
web:
	cd web && bun install && bun run build

# Verify the cross-compilation toolchain is available
check-linker:
	@which $(LINKER) > /dev/null 2>&1 || { \
		echo ""; \
		echo "ERROR: Cross-linker '$(LINKER)' not found in PATH"; \
		echo ""; \
		echo "Install the cross-compilation toolchain:"; \
		echo ""; \
		echo "  macOS (Homebrew):"; \
		echo "    brew tap messense/macos-cross-toolchains"; \
		echo "    brew install $(TARGET)"; \
		echo ""; \
		echo "  Ubuntu/Debian:"; \
		echo "    sudo apt-get install gcc-aarch64-linux-gnu"; \
		echo ""; \
		echo "Then ensure rustup has the target:"; \
		echo "    rustup target add $(TARGET)"; \
		echo ""; \
		exit 1; \
	}
	@echo "Cross-linker '$(LINKER)' found: $$(which $(LINKER))"

# Build the supervisor binary for Linux ARM64
supervisor: check-linker
	CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=$(LINKER) \
		cargo build -p tasks-supervisor --release --target $(TARGET)
	@echo "Built: $(SUPERVISOR_BIN)"

# Build the container image (requires supervisor binary)
container-image: supervisor
	@test -f $(SUPERVISOR_BIN) || { echo "ERROR: $(SUPERVISOR_BIN) not found"; exit 1; }
	container build --dns 8.8.8.8 -f src/runtime/Dockerfile -t $(CONTAINER_IMAGE) .
	@echo "Built container image: $(CONTAINER_IMAGE)"

# Clean build artifacts
clean:
	cargo clean
	@echo "Cleaned build artifacts"
