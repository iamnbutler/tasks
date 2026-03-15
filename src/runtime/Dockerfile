# Stage 1: Build the supervisor binary
FROM rust:1.85-slim AS builder

WORKDIR /build

# Copy just the workspace manifests and supervisor crate first (cache deps).
COPY Cargo.toml Cargo.lock* ./
COPY crates/supervisor/Cargo.toml crates/supervisor/Cargo.toml

# Create dummy source to build deps.
RUN mkdir -p crates/supervisor/src && echo 'fn main() {}' > crates/supervisor/src/main.rs
RUN cargo build -p tasks-supervisor --release 2>/dev/null || true

# Copy real source and build.
COPY crates/supervisor/src crates/supervisor/src
RUN cargo build -p tasks-supervisor --release

# Stage 2: Runtime image
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# System dependencies
RUN apt-get update && apt-get install -y \
    git \
    curl \
    ssh \
    build-essential \
    ca-certificates \
    unzip \
    && rm -rf /var/lib/apt/lists/*

# GitHub CLI
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    -o /usr/share/keyrings/githubcli-archive-keyring.gpg && \
    echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    > /etc/apt/sources.list.d/github-cli.list && \
    apt-get update && apt-get install -y gh && \
    rm -rf /var/lib/apt/lists/*

# Node.js (required for Claude Code)
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs && \
    rm -rf /var/lib/apt/lists/*

# Claude Code
RUN npm install -g @anthropic-ai/claude-code

# Rust (minimal profile — for Rust-based projects the agent works on)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# Supervisor binary from build stage.
COPY --from=builder /build/target/release/tasks-supervisor /usr/local/bin/supervisor

# Workspace directory for cloned repos
RUN mkdir -p /workspace

ENTRYPOINT ["supervisor"]
