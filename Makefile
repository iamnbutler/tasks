# Build configuration
TARGET := aarch64-unknown-linux-gnu
SUPERVISOR_CRATE := tasks-supervisor
SUPERVISOR_BINARY := target/$(TARGET)/release/$(SUPERVISOR_CRATE)
IMAGE_NAME := tasks-agent
IMAGE_TAG := latest

# Detect host OS and architecture
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

# Check if we need cross-compilation
ifeq ($(UNAME_S),Linux)
  ifeq ($(UNAME_M),aarch64)
    # Native build on Linux aarch64
    CARGO_BUILD := cargo build -p $(SUPERVISOR_CRATE) --release --target $(TARGET)
  else
    # Cross-compile from Linux x86_64
    CARGO_BUILD := cargo build -p $(SUPERVISOR_CRATE) --release --target $(TARGET)
  endif
else ifeq ($(UNAME_S),Darwin)
  # Cross-compile from macOS
  CARGO_BUILD := cargo build -p $(SUPERVISOR_CRATE) --release --target $(TARGET)
endif

.PHONY: all supervisor container-image clean check-target

all: container-image

# Build the supervisor binary for Linux aarch64
supervisor: check-target
	$(CARGO_BUILD)
	@echo "Built supervisor at $(SUPERVISOR_BINARY)"

# Ensure the target is installed
check-target:
	@rustup target list --installed | grep -q $(TARGET) || \
		(echo "Installing $(TARGET) target..." && rustup target add $(TARGET))

# Build the container image (requires supervisor binary)
container-image: supervisor
	container build --dns 8.8.8.8 -f src/runtime/Dockerfile -t $(IMAGE_NAME):$(IMAGE_TAG) .

# Clean build artifacts
clean:
	cargo clean

# Help target
help:
	@echo "Tasks Agent Build System"
	@echo ""
	@echo "Targets:"
	@echo "  supervisor       - Build the supervisor binary for Linux aarch64"
	@echo "  container-image  - Build the container image (default)"
	@echo "  clean            - Remove build artifacts"
	@echo ""
	@echo "Environment:"
	@echo "  Host OS:         $(UNAME_S)"
	@echo "  Host Arch:       $(UNAME_M)"
	@echo "  Target:          $(TARGET)"
