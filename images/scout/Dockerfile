# Scout image for Tasks (tagged agent:v1)
# vm-pool agent image (Claude Code + dev tools) with scout-supervisor as PID 1.
#
# Runs as the non-root `agent` user (inherited from the base image) — required:
# Claude Code refuses --dangerously-skip-permissions as root.

FROM vm-pool-agent:latest

USER root
COPY scout-supervisor /usr/local/bin/scout-supervisor
RUN chmod +x /usr/local/bin/scout-supervisor

# cargo-nextest, from the official prebuilt tarball. Without it `make test`
# hard-fails with an install hint and pushes the agent onto bare
# `cargo test --workspace` — the exact command whose parallel links OOM.
RUN arch="$(uname -m)" && \
    case "$arch" in \
      aarch64|arm64) url=https://get.nextest.dev/latest/linux-arm ;; \
      x86_64) url=https://get.nextest.dev/latest/linux ;; \
      *) echo "no nextest build for $arch" >&2; exit 1 ;; \
    esac && \
    curl -LsSf "$url" | tar zxf - -C /usr/local/bin

USER agent
WORKDIR /home/agent

# The base image installs Rust for root only; scouts build as `agent`.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/home/agent/.cargo/bin:${PATH}"

# The floor for a container someone starts by hand — including one booted at
# vm-pool's 2 GB default. The server derives a real job count from the memory
# it gives each VM and injects it as `-e CARGO_BUILD_JOBS`, overriding this;
# what this line guarantees is that nothing ever links at cargo's own default
# of one job per CPU, which is how a 4 CPU / 4 GB VM got its linker OOM-killed.
ENV CARGO_BUILD_JOBS=1

# Headless agent invocation. The prompt arrives on stdin; SPEC.md is the
# artifact. Credentials (ANTHROPIC_API_KEY) are injected per-VM, never baked
# into the image.
# stream-json turns stdout into typed JSON-lines (assistant messages, thinking,
# tool calls with inputs, tool results, and a final result record with usage and
# cost) which the server persists as the session transcript. --verbose is
# mandatory: `--print --output-format stream-json` without it exits with
# "requires --verbose" and every scout fails at the agent step.
ENV SCOUT_AGENT_CMD="claude --print --output-format stream-json --verbose --dangerously-skip-permissions"

# How often the supervisor polls NOTES.md and streams it back as a checkpoint.
# Set here rather than in the server's env because it is read *inside* the VM.
# 30s is far finer than the failure it insures against (a whole scout run
# lost), and a scout's VM is destroyed at the deadline — whatever the host
# already received is all there will ever be.
ENV SCOUT_CHECKPOINT_INTERVAL_SECS=30

ENTRYPOINT ["/usr/local/bin/scout-supervisor"]
