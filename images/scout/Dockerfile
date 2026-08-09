# Scout image for Tasks (tagged agent:v1)
# vm-pool agent image (Claude Code + dev tools) with scout-supervisor as PID 1.
#
# Runs as the non-root `agent` user (inherited from the base image) — required:
# Claude Code refuses --dangerously-skip-permissions as root.

FROM vm-pool-agent:latest

USER root
COPY scout-supervisor /usr/local/bin/scout-supervisor
RUN chmod +x /usr/local/bin/scout-supervisor

USER agent
WORKDIR /home/agent

# The base image installs Rust for root only; scouts build as `agent`.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
ENV PATH="/home/agent/.cargo/bin:${PATH}"

# Headless agent invocation. The prompt arrives on stdin; SPEC.md is the
# artifact. Credentials (ANTHROPIC_API_KEY) are injected per-VM, never baked
# into the image.
ENV SCOUT_AGENT_CMD="claude --print --dangerously-skip-permissions"

ENTRYPOINT ["/usr/local/bin/scout-supervisor"]
