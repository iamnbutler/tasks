# Getting Started

This guide walks you through setting up and running Tasks for the first time.

## Prerequisites

- **Rust** (stable toolchain)
- **Bun** (for web frontend)
- **GitHub Token** with repo access
- **Anthropic API Key** for Claude

### macOS Additional Requirements

For cross-compiling the container supervisor:

```bash
# Install cross-compiler
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-gnu

# Add Rust target
rustup target add aarch64-unknown-linux-gnu
```

### Linux Additional Requirements

```bash
# Install cross-compiler (Debian/Ubuntu)
sudo apt install gcc-aarch64-linux-gnu

# Add Rust target
rustup target add aarch64-unknown-linux-gnu
```

## Installation

### 1. Clone the Repository

```bash
git clone https://github.com/iamnbutler/tasks.git
cd tasks
```

### 2. Configure Environment

Create a `.env` file at the project root:

```env
GITHUB_TOKEN=ghp_your_token_here
ANTHROPIC_API_KEY=sk-ant-your_key_here
```

### 3. Build the Project

```bash
# Build the Rust backend
cargo build --release

# Build the web frontend
bun install && bun web build
```

### 4. Build Container Image (Optional)

For running agent sessions in isolated containers:

```bash
make container-image
```

## Running Tasks

### Headless Mode

```bash
cargo run -- run
```

### With Web UI

```bash
cargo run -- run --web
```

The web interface will be available at `http://localhost:4800`.

## Adding a Project

```bash
cargo run -- add-project owner/repo
```

This adds a GitHub repository for Tasks to monitor.

## Next Steps

- [Architecture Overview](architecture.md) - Understand the system design
- [CLI Reference](cli-reference.md) - All available commands
- [API Reference](api-reference.md) - REST API documentation
- [Web UI Guide](web-ui.md) - Using the web interface

---

*This documentation is automatically maintained. Last updated: 2026-03-23*
