# Automation image for vm-pool
# Base + minimal tooling for headless CI-style tasks

FROM vm-pool-base:latest

USER root

# Install additional CI tools
RUN apt-get update && apt-get install -y \
    jq \
    yq \
    && rm -rf /var/lib/apt/lists/*

USER agent
WORKDIR /home/agent

# No Claude Code, no interactive tools
# Optimized for fast, headless execution

ENTRYPOINT ["/usr/local/bin/supervisor"]
