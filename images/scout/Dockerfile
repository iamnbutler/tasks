# Scout image for Tasks (tagged agent:v1)
# vm-pool agent image (Claude Code + dev tools) with scout-supervisor as PID 1.
#
# Runs as the non-root `agent` user (inherited from the base image) — required:
# Claude Code refuses --dangerously-skip-permissions as root.

FROM vm-pool-agent:latest

USER root
COPY scout-supervisor /usr/local/bin/scout-supervisor
RUN chmod +x /usr/local/bin/scout-supervisor

# cargo-nextest, pinned, from the release archive it is actually published to.
# Without it `make test` hard-fails with an install hint and pushes the agent
# onto bare `cargo test --workspace` — the exact command whose parallel links
# OOM.
#
# Not `get.nextest.dev`: that redirector now answers NXDOMAIN, and since the
# fetch was piped straight into `tar`, the build died reporting `tar: Child
# returned status 1` — a name that would not resolve, wearing an archive
# error's clothes. Every image build failed on it, which is how a fortnight of
# supervisor fixes stayed undeployed while the fixes themselves sat on main.
# A pinned release URL cannot vanish out from under a rebuild, and landing the
# download in a file first means the next failure names itself.
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

# app-gpui's build dependencies. Without them `cd app-gpui && cargo test`
# fails in a build script — `yeslogic-fontconfig-sys` probes pkg-config for
# fontconfig, and linking the test binary additionally wants xcb and
# xkbcommon(-x11). The Makefile's `app-check` / `app-test` can work around
# their absence (RUST_FONTCONFIG_DLOPEN plus empty stub .so's) but the plain
# command an agent reaches for first cannot, and the workaround's RUSTFLAGS
# rebuild the whole gpui tree over a hand-run `cargo test`.
#
# The trade: ~23 MB installed (27 new packages, 7 upgrades on arm64 — pkgconf,
# freetype, png, zlib, expat, x11proto, xcb-xkb, xkb-data among them) and
# ~20s of image build, in an image that already carries Rust, Node, Bun, gh
# and Claude Code. Cheap against a crate whose every change so far merged
# uncompiled.
#
# apt-get update is required, not decorative: the base image deletes
# /var/lib/apt/lists.
RUN apt-get update && apt-get install -y \
    pkg-config \
    libfontconfig-dev \
    libxkbcommon-dev \
    libxkbcommon-x11-dev \
    libxcb1-dev \
    && rm -rf /var/lib/apt/lists/*

USER agent
WORKDIR /home/agent

# The base image installs Rust for root only; scouts build as `agent`.
#
# `--profile minimal` leaves out rustfmt and clippy, and `cargo fmt` +
# `cargo clippy --workspace --all-targets` are what this repo requires clean
# before committing — without the components they fail with "not installed
# for the toolchain", in the one place that cannot install them mid-run.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt -c clippy
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

# How many times the supervisor re-invokes a dead agent with
# `--resume <session_id>` when its API connection dropped mid-response (#845).
# Set here rather than in the server's env for the same reason as the line
# above: it is read *inside* the VM. `0` disables resuming.
#
# The resume happens in this VM, so the conversation, the worktree and NOTES.md
# all survive it — a host-side retry would get a new VM and a fresh clone and
# keep none of them. Only a transport death is retried; an OOM kill, a
# deallocated VM and an agent that concluded are all left alone. See
# crates/tasks-protocol/src/agent_run.rs.
ENV SCOUT_MAX_RESUMES=2

ENTRYPOINT ["/usr/local/bin/scout-supervisor"]
