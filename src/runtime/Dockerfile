# Single-stage runtime image
# The supervisor binary is cross-compiled on the host via `make supervisor`
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

# Rust (minimal profile - for Rust-based projects the agent works on)
RUN su agent -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal'
ENV PATH="/home/agent/.cargo/bin:${PATH}"

# Supervisor binary (cross-compiled on host, runs as root PID 1, spawns agent as non-root)
COPY target/aarch64-unknown-linux-gnu/release/tasks-supervisor /usr/local/bin/supervisor

# Pre-accept Claude Code ToS so it doesn't hang on first run
RUN mkdir -p /home/agent/.claude && \
    echo '{"hasCompletedOnboarding":true,"hasAcknowledgedCostThreshold":true}' > /home/agent/.claude/settings.json && \
    chown -R agent:agent /home/agent/.claude

# Workspace directory for cloned repos (owned by agent user)
RUN mkdir -p /workspace && chown agent:agent /workspace

ENTRYPOINT ["supervisor"]
