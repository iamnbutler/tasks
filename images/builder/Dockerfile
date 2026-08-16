# Builder image for Tasks (tagged builder:v1)
# vm-pool agent image (Claude Code + dev tools) with builder-supervisor as
# PID 1. Same base as the scout image; only the supervisor and the agent's
# job differ — a Builder's deliverable is the branch, shipped back to the
# server as a git bundle. No push credential ever enters this image or VM.

FROM vm-pool-agent:latest

USER root
COPY builder-supervisor /usr/local/bin/builder-supervisor
RUN chmod +x /usr/local/bin/builder-supervisor

# cargo-nextest, pinned — see images/scout/Dockerfile for why an agent that
# cannot run `make test` is a memory problem, and for why this no longer goes
# through `get.nextest.dev`.
ARG NEXTEST_VERSION=0.9.143
RUN arch="$(uname -m)" && \
    case "$arch" in \
      aarch64|arm64) target=aarch64-unknown-linux-gnu ;; \
      x86_64) target=x86_64-unknown-linux-gnu ;; \
      *) echo "no nextest build for $arch" >&2; exit 1 ;; \
    esac && \
    curl -LsSf -o /tmp/nextest.tar.gz \
      "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-${NEXTEST_VERSION}/cargo-nextest-${NEXTEST_VERSION}-${target}.tar.gz" && \
    tar zxf /tmp/nextest.tar.gz -C /usr/local/bin && \
    rm /tmp/nextest.tar.gz && \
    test -x /usr/local/bin/cargo-nextest

# app-gpui's build dependencies — see images/scout/Dockerfile for what each
# package is for and what the layer costs. A Builder needs them more than a
# Scout does: it is the one that has to leave an app-gpui spec implemented and
# tested, not merely explored.
RUN apt-get update && apt-get install -y \
    pkg-config \
    libfontconfig-dev \
    libxkbcommon-dev \
    libxkbcommon-x11-dev \
    libxcb1-dev \
    && rm -rf /var/lib/apt/lists/*

USER agent
WORKDIR /home/agent

# The base image installs Rust for root only; builders build as `agent`.
# `-c rustfmt -c clippy` because `--profile minimal` ships neither, and both
# are required clean before committing — see images/scout/Dockerfile.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt -c clippy
ENV PATH="/home/agent/.cargo/bin:${PATH}"

# Floor for a hand-started container; the server's per-VM `-e CARGO_BUILD_JOBS`
# (derived from the VM's memory) overrides it. See images/scout/Dockerfile.
ENV CARGO_BUILD_JOBS=1

# Headless agent invocation — same shape as the scout image; see
# images/scout/Dockerfile for why --verbose is mandatory with stream-json.
# Credentials (ANTHROPIC_API_KEY) are injected per-VM, never baked in.
ENV BUILDER_AGENT_CMD="claude --print --output-format stream-json --verbose --dangerously-skip-permissions"

# Resumes allowed after a dropped API connection (#845) — read inside the VM,
# so it is set here rather than in the server's env. `0` disables resuming.
# See images/scout/Dockerfile; it matters more here, because a Builder's
# worktree is the implementation and only an in-VM resume keeps it.
ENV BUILDER_MAX_RESUMES=2

ENTRYPOINT ["/usr/local/bin/builder-supervisor"]
