# Stage 1: Build the supervisor binary
FROM rust:1.85-slim AS builder

WORKDIR /build

# Copy workspace manifests.
COPY Cargo.toml Cargo.lock* ./

# Copy all crate Cargo.toml files (workspace glob requires them to exist).
COPY crates/app/Cargo.toml crates/app/Cargo.toml
COPY crates/events/Cargo.toml crates/events/Cargo.toml
COPY crates/github/Cargo.toml crates/github/Cargo.toml
COPY crates/models/Cargo.toml crates/models/Cargo.toml
COPY crates/runtime/Cargo.toml crates/runtime/Cargo.toml
COPY crates/server/Cargo.toml crates/server/Cargo.toml
COPY crates/session/Cargo.toml crates/session/Cargo.toml
COPY crates/store/Cargo.toml crates/store/Cargo.toml
COPY crates/supervisor/Cargo.toml crates/supervisor/Cargo.toml

# Create dummy lib/main files so cargo can resolve the workspace and cache deps.
RUN for dir in crates/*/; do \
      mkdir -p "$dir/src"; \
      name=$(basename "$dir"); \
      if [ "$name" = "app" ] || [ "$name" = "supervisor" ]; then \
        echo 'fn main() {}' > "$dir/src/main.rs"; \
      else \
        echo '' > "$dir/src/lib.rs"; \
      fi; \
    done

RUN cargo build -p tasks-supervisor --release 2>/dev/null || true

# Copy real supervisor source and rebuild (remove cached dummy binary to force recompile).
COPY crates/supervisor/src crates/supervisor/src
RUN touch crates/supervisor/src/main.rs && cargo build -p tasks-supervisor --release

# Stage 2: Runtime image
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# System dependencies
RUN apt-get update && apt-get install -y \
    git \
    curl \
    ssh \
    sudo \
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

# Create non-root user (Claude Code refuses --dangerously-skip-permissions as root)
RUN useradd -m -s /bin/bash agent && \
    echo 'agent ALL=(ALL) NOPASSWD:ALL' >> /etc/sudoers && \
    echo 'Defaults env_keep += "ANTHROPIC_API_KEY GITHUB_TOKEN AGENT_CMD AGENT_ARGS"' >> /etc/sudoers && \
    echo 'Defaults secure_path="/home/agent/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"' >> /etc/sudoers

# Rust (minimal profile — for Rust-based projects the agent works on)
RUN su agent -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal'
ENV PATH="/home/agent/.cargo/bin:${PATH}"

# Supervisor binary from build stage (runs as root PID 1, spawns agent as non-root).
COPY --from=builder /build/target/release/tasks-supervisor /usr/local/bin/supervisor

# Pre-accept Claude Code ToS so it doesn't hang on first run
RUN mkdir -p /home/agent/.claude && \
    echo '{"hasCompletedOnboarding":true,"hasAcknowledgedCostThreshold":true}' > /home/agent/.claude/settings.json && \
    chown -R agent:agent /home/agent/.claude

# Workspace directory for cloned repos (owned by agent user)
RUN mkdir -p /workspace && chown agent:agent /workspace

ENTRYPOINT ["supervisor"]
