# Agent image for vm-pool
# Base + Claude Code + dev tools for interactive agent work

FROM vm-pool-base:latest

USER root

# Install Claude Code
RUN npm install -g @anthropic-ai/claude-code

# Accept Claude Code ToS for the agent user
USER agent
RUN mkdir -p ~/.claude && \
    echo '{"hasCompletedOnboarding": true, "hasAcceptedTermsOfService": true}' > ~/.claude/settings.json

# Pre-configure git
RUN git config --global user.name "Tasks Agent" && \
    git config --global user.email "agent@tasks.local" && \
    git config --global init.defaultBranch main

USER agent
WORKDIR /home/agent

ENTRYPOINT ["/usr/local/bin/supervisor"]
