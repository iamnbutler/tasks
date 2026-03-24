#!/usr/bin/env bash
#
# tasks-runner.sh — Wrapper script for running the Tasks server with self-update support.
#
# This script runs the server in a loop. When the server exits with code 100,
# it indicates an update is available. The script then:
# 1. Pulls the latest changes from origin/main
# 2. Reads the rebuild scope from .update-scope
# 3. Rebuilds the necessary components
# 4. Restarts the server
#
# Usage:
#   ./scripts/tasks-runner.sh [--web] [args...]
#
# Environment:
#   TASKS_DATA_DIR — Data directory (default: ~/.local/state/tasks)
#

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${BLUE}[tasks-runner]${NC} $*"
}

log_success() {
    echo -e "${GREEN}[tasks-runner]${NC} $*"
}

log_warn() {
    echo -e "${YELLOW}[tasks-runner]${NC} $*"
}

log_error() {
    echo -e "${RED}[tasks-runner]${NC} $*"
}

# Get data directory
DATA_DIR="${TASKS_DATA_DIR:-$HOME/.local/state/tasks}"
SCOPE_FILE="$DATA_DIR/.update-scope"

# Exit code indicating update needed
UPDATE_EXIT_CODE=100

# Read rebuild scope from file
read_scope() {
    if [[ -f "$SCOPE_FILE" ]]; then
        cat "$SCOPE_FILE"
    else
        echo "all"
    fi
}

# Clean up scope file after reading
cleanup_scope_file() {
    rm -f "$SCOPE_FILE"
}

# Pull latest changes
pull_updates() {
    log_info "Pulling latest changes from origin/main..."

    if ! git fetch origin main; then
        log_error "Failed to fetch from origin"
        return 1
    fi

    if ! git merge --ff-only origin/main; then
        log_error "Failed to merge origin/main (non-fast-forward)"
        log_error "Manual intervention required. Exiting."
        return 1
    fi

    log_success "Successfully pulled updates"
}

# Rebuild based on scope
rebuild() {
    local scope="$1"
    log_info "Rebuilding with scope: $scope"

    case "$scope" in
        none)
            log_info "No rebuild needed"
            ;;
        frontend)
            log_info "Rebuilding frontend..."
            if command -v bun &> /dev/null; then
                (cd web && bun install && bun run build)
            else
                log_warn "bun not found, skipping frontend rebuild"
            fi
            ;;
        server)
            log_info "Rebuilding server..."
            cargo build --release --package tasks-app
            ;;
        container)
            log_info "Rebuilding container image..."
            make container-image
            ;;
        server_and_frontend)
            log_info "Rebuilding server and frontend..."
            cargo build --release --package tasks-app
            if command -v bun &> /dev/null; then
                (cd web && bun install && bun run build)
            else
                log_warn "bun not found, skipping frontend rebuild"
            fi
            ;;
        server_and_container)
            log_info "Rebuilding server and container..."
            cargo build --release --package tasks-app
            make container-image
            ;;
        all)
            log_info "Full rebuild..."
            cargo build --release --package tasks-app
            if command -v bun &> /dev/null; then
                (cd web && bun install && bun run build)
            else
                log_warn "bun not found, skipping frontend rebuild"
            fi
            make container-image
            ;;
        *)
            log_warn "Unknown scope '$scope', performing full rebuild"
            cargo build --release --package tasks-app
            if command -v bun &> /dev/null; then
                (cd web && bun install && bun run build)
            else
                log_warn "bun not found, skipping frontend rebuild"
            fi
            make container-image
            ;;
    esac

    log_success "Rebuild complete"
}

# Main loop
main() {
    log_info "Starting Tasks server runner"
    log_info "Data directory: $DATA_DIR"

    # Ensure data directory exists
    mkdir -p "$DATA_DIR"

    while true; do
        log_info "Starting server..."

        # Run the server, capturing exit code
        set +e
        cargo run --release --package tasks-app -- run "$@"
        exit_code=$?
        set -e

        if [[ $exit_code -eq $UPDATE_EXIT_CODE ]]; then
            log_info "Server requested update restart (exit code $UPDATE_EXIT_CODE)"

            # Read the scope before cleaning up
            scope=$(read_scope)
            cleanup_scope_file

            # Pull and rebuild
            if pull_updates; then
                rebuild "$scope"
                log_success "Update complete, restarting server..."
            else
                log_error "Update failed, restarting server with current version..."
            fi

            # Small delay before restart
            sleep 2
        else
            if [[ $exit_code -eq 0 ]]; then
                log_info "Server exited normally"
            else
                log_error "Server exited with code $exit_code"
            fi
            break
        fi
    done

    log_info "Tasks runner exiting"
    exit $exit_code
}

# Run main with all arguments
main "$@"
