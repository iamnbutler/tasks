#!/usr/bin/env bash
# tasks-runner.sh — Wrapper script for self-updating Tasks platform
#
# This script runs the Tasks server in a loop, handling automatic updates.
# When the server exits with code 100, it pulls changes, rebuilds based on
# the scope file, and restarts.
#
# Usage:
#   ./scripts/tasks-runner.sh [--web] [--auto-update]
#
# Options:
#   --web          Enable the web UI
#   --auto-update  Enable automatic update application (TASKS_UPDATE_AUTO_APPLY=true)
#
# Environment variables:
#   TASKS_DATA_DIR              Data directory (default: ~/.local/state/tasks)
#   TASKS_UPDATE_CHECK_ENABLED  Enable update checking (default: true)
#   TASKS_UPDATE_CHECK_INTERVAL Update check interval in seconds (default: 300)
#   TASKS_UPDATE_AUTO_APPLY     Auto-apply updates (default: false)
#   TASKS_UPDATE_SESSION_TIMEOUT Session stop timeout in seconds (default: 300)

set -euo pipefail

# Exit code indicating update ready
EXIT_CODE_UPDATE=100

# Parse arguments
ARGS=()
for arg in "$@"; do
    case "$arg" in
        --auto-update)
            export TASKS_UPDATE_AUTO_APPLY=true
            ;;
        *)
            ARGS+=("$arg")
            ;;
    esac
done

# Data directory for scope file
DATA_DIR="${TASKS_DATA_DIR:-$HOME/.local/state/tasks}"
SCOPE_FILE="$DATA_DIR/.update-scope"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${BLUE}[tasks-runner]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[tasks-runner]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[tasks-runner]${NC} $1"
}

log_error() {
    echo -e "${RED}[tasks-runner]${NC} $1"
}

# Build based on scope
build_scope() {
    local scope="$1"
    local build_failed=false

    log_info "Building components: $scope"

    # Server rebuild (cargo build)
    if [[ "$scope" == *"server"* ]]; then
        log_info "Building server..."
        if ! cargo build --release --package tasks-app; then
            log_error "Server build failed"
            build_failed=true
        else
            log_success "Server built successfully"
        fi
    fi

    # Container rebuild (make container-image)
    if [[ "$scope" == *"container"* ]]; then
        log_info "Building container image..."
        if ! make container-image; then
            log_error "Container build failed"
            build_failed=true
        else
            log_success "Container image built successfully"
        fi
    fi

    # Frontend rebuild (bun web build)
    if [[ "$scope" == *"frontend"* ]]; then
        log_info "Building frontend..."
        if command -v bun &> /dev/null; then
            if ! (cd web && bun install && bun run build); then
                log_error "Frontend build failed"
                build_failed=true
            else
                log_success "Frontend built successfully"
            fi
        else
            log_warn "bun not found, skipping frontend build"
        fi
    fi

    if $build_failed; then
        return 1
    fi
    return 0
}

# Main loop
main() {
    log_info "Starting Tasks runner (auto-update wrapper)"
    log_info "Data directory: $DATA_DIR"
    log_info "Update auto-apply: ${TASKS_UPDATE_AUTO_APPLY:-false}"

    while true; do
        log_info "Starting Tasks server..."

        # Run the server
        set +e
        cargo run --release --package tasks-app -- run "${ARGS[@]}"
        exit_code=$?
        set -e

        log_info "Server exited with code: $exit_code"

        # Check for update exit code
        if [[ $exit_code -eq $EXIT_CODE_UPDATE ]]; then
            log_info "Update requested, pulling changes..."

            # Pull latest changes
            if ! git pull origin main; then
                log_error "Failed to pull changes, retrying in 60 seconds..."
                sleep 60
                continue
            fi

            # Read scope file
            if [[ -f "$SCOPE_FILE" ]]; then
                scope=$(cat "$SCOPE_FILE")
                log_info "Update scope: $scope"
                rm -f "$SCOPE_FILE"
            else
                # Default to full rebuild if no scope file
                scope="server,container,frontend"
                log_warn "No scope file found, defaulting to full rebuild"
            fi

            # Build based on scope
            if ! build_scope "$scope"; then
                log_error "Build failed, retrying in 60 seconds..."
                sleep 60
                continue
            fi

            log_success "Update complete, restarting server..."
            continue
        fi

        # Any other exit code — break the loop
        if [[ $exit_code -ne 0 ]]; then
            log_error "Server exited with error code $exit_code"
            exit $exit_code
        fi

        log_info "Server exited normally"
        break
    done
}

# Handle signals
trap 'log_info "Received signal, shutting down..."; exit 0' SIGINT SIGTERM

main
