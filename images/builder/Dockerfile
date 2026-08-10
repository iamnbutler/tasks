# Builder image for Tasks (tagged builder:v1)
# vm-pool agent image (Claude Code + dev tools) with builder-supervisor as
# PID 1. Same base as the scout image; only the supervisor and the agent's
# job differ — a Builder's deliverable is the branch, shipped back to the
# server as a git bundle. No push credential ever enters this image or VM.

FROM vm-pool-agent:latest

USER root
COPY builder-supervisor /usr/local/bin/builder-supervisor
RUN chmod +x /usr/local/bin/builder-supervisor

USER agent
WORKDIR /home/agent

# The base image installs Rust for root only; builders build as `agent`.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/home/agent/.cargo/bin:${PATH}"

# Headless agent invocation — same shape as the scout image; see
# images/scout/Dockerfile for why --verbose is mandatory with stream-json.
# Credentials (ANTHROPIC_API_KEY) are injected per-VM, never baked in.
ENV BUILDER_AGENT_CMD="claude --print --output-format stream-json --verbose --dangerously-skip-permissions"

ENTRYPOINT ["/usr/local/bin/builder-supervisor"]
