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
# Phase 4 robustness features:
# - Build failure handling with binary backup and fallback
# - Network failure handling with exponential backoff
# - Partial update recovery via state files
# - Signal handling (SIGTERM, SIGINT)
# - PID file management
# - Logging to file with rotation
# - Health check after restart
#
# Usage:
#   ./scripts/tasks-runner.sh [--web] [args...]
#
# Environment:
#   TASKS_DATA_DIR        — Data directory (default: ~/.local/state/tasks)
#   TASKS_NET_MAX_RETRIES — Max network retry attempts (default: 10)
#   TASKS_RETRY_DELAY     — Initial retry delay in seconds (default: 5)
#   TASKS_MAX_RETRY_DELAY — Maximum retry delay in seconds (default: 300)
#   TASKS_HEALTH_TIMEOUT  — Health check timeout in seconds (default: 30)
#   TASKS_LOG_MAX_SIZE    — Max log file size in bytes (default: 10485760 = 10MB)
#

set -euo pipefail

# Colors for output (only when stdout is a terminal)
if [[ -t 1 ]]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BLUE='\033[0;34m'
    NC='\033[0m'
else
    RED=''
    GREEN=''
    YELLOW=''
    BLUE=''
    NC=''
fi

# Configuration
DATA_DIR="${TASKS_DATA_DIR:-$HOME/.local/state/tasks}"
LOG_FILE="$DATA_DIR/runner.log"
PID_FILE="$DATA_DIR/tasks.pid"
SCOPE_FILE="$DATA_DIR/.update-scope"
STATE_FILE="$DATA_DIR/.update-state"
BACKUP_DIR="$DATA_DIR/backup"

# Retry configuration
MAX_RETRIES="${TASKS_NET_MAX_RETRIES:-10}"
RETRY_DELAY="${TASKS_RETRY_DELAY:-5}"
MAX_RETRY_DELAY="${TASKS_MAX_RETRY_DELAY:-300}"
HEALTH_TIMEOUT="${TASKS_HEALTH_TIMEOUT:-30}"
LOG_MAX_SIZE="${TASKS_LOG_MAX_SIZE:-10485760}"

# Exit code indicating update needed
UPDATE_EXIT_CODE=100

# Track server PID
SERVER_PID=""

# Track consecutive network failures
NETWORK_FAILURES=0

# Logging functions
timestamp() {
    date '+%Y-%m-%d %H:%M:%S'
}

log() {
    local level="$1"
    shift
    local msg="[$(timestamp)] [$level] $*"
    echo -e "$msg" >> "$LOG_FILE"
    case "$level" in
        INFO)  echo -e "${BLUE}[tasks-runner]${NC} $*" ;;
        OK)    echo -e "${GREEN}[tasks-runner]${NC} $*" ;;
        WARN)  echo -e "${YELLOW}[tasks-runner]${NC} $*" ;;
        ERROR) echo -e "${RED}[tasks-runner]${NC} $*" ;;
    esac
}

log_info()  { log INFO "$@"; }
log_success() { log OK "$@"; }
log_warn()  { log WARN "$@"; }
log_error() { log ERROR "$@"; }

# Rotate log file if too large
rotate_log() {
    if [[ -f "$LOG_FILE" ]]; then
        local size
        size=$(stat -f%z "$LOG_FILE" 2>/dev/null || stat -c%s "$LOG_FILE" 2>/dev/null || echo 0)
        if [[ $size -gt $LOG_MAX_SIZE ]]; then
            mv "$LOG_FILE" "$LOG_FILE.1"
            log_info "Log rotated (was $size bytes)"
        fi
    fi
}

# PID file management
write_pid() {
    echo $$ > "$PID_FILE"
    log_info "PID file written: $$"
}

check_stale_pid() {
    if [[ -f "$PID_FILE" ]]; then
        local old_pid
        old_pid=$(cat "$PID_FILE")
        if kill -0 "$old_pid" 2>/dev/null; then
            log_error "Another instance is running (PID $old_pid)"
            exit 1
        else
            log_warn "Removing stale PID file (PID $old_pid not running)"
            rm -f "$PID_FILE"
        fi
    fi
}

cleanup_pid() {
    rm -f "$PID_FILE"
}

# Signal handling
setup_signal_handlers() {
    trap 'handle_signal SIGTERM' SIGTERM
    trap 'handle_signal SIGINT' SIGINT
    trap 'cleanup_pid' EXIT
}

handle_signal() {
    local signal="$1"
    log_info "Received $signal"

    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        log_info "Forwarding $signal to server (PID $SERVER_PID)"
        kill -"${signal#SIG}" "$SERVER_PID" 2>/dev/null || true

        # Wait for server to exit gracefully
        local timeout=30
        while kill -0 "$SERVER_PID" 2>/dev/null && [[ $timeout -gt 0 ]]; do
            sleep 1
            ((timeout--))
        done

        if kill -0 "$SERVER_PID" 2>/dev/null; then
            log_warn "Server did not exit, sending SIGKILL"
            kill -9 "$SERVER_PID" 2>/dev/null || true
        fi
    fi

    cleanup_pid
    exit 0
}

# Update state management
write_state() {
    echo "$1" > "$STATE_FILE"
}

read_state() {
    if [[ -f "$STATE_FILE" ]]; then
        cat "$STATE_FILE"
    else
        echo "none"
    fi
}

cleanup_state() {
    rm -f "$STATE_FILE" "$SCOPE_FILE"
}

# Read rebuild scope from file
read_scope() {
    if [[ -f "$SCOPE_FILE" ]]; then
        cat "$SCOPE_FILE"
    else
        echo "all"
    fi
}

# Network operations with exponential backoff
git_fetch_with_retry() {
    local attempt=0
    local delay=$RETRY_DELAY

    while [[ $attempt -lt $MAX_RETRIES ]]; do
        if git fetch origin main 2>&1; then
            NETWORK_FAILURES=0
            return 0
        fi

        ((attempt++))
        ((NETWORK_FAILURES++))

        if [[ $attempt -lt $MAX_RETRIES ]]; then
            # Only log/emit event on first failure or after recovery
            if [[ $NETWORK_FAILURES -eq 1 ]]; then
                log_warn "Network failure, will retry with backoff"
            fi

            log_warn "git fetch failed (attempt $attempt/$MAX_RETRIES), retrying in ${delay}s..."
            sleep "$delay"

            # Exponential backoff
            delay=$((delay * 2))
            if [[ $delay -gt $MAX_RETRY_DELAY ]]; then
                delay=$MAX_RETRY_DELAY
            fi
        fi
    done

    log_error "git fetch failed after $MAX_RETRIES attempts"
    return 1
}

# Pull latest changes
pull_updates() {
    log_info "Pulling latest changes from origin/main..."
    write_state "fetching"

    if ! git_fetch_with_retry; then
        log_error "Failed to fetch from origin"
        return 1
    fi

    write_state "merging"

    if ! git merge --ff-only origin/main; then
        log_error "Failed to merge origin/main (non-fast-forward)"
        log_error "Manual intervention required"
        return 1
    fi

    log_success "Successfully pulled updates"
    return 0
}

# Binary backup for rollback
backup_binary() {
    local binary_path="target/release/tasks-app"

    if [[ -f "$binary_path" ]]; then
        mkdir -p "$BACKUP_DIR"
        cp "$binary_path" "$BACKUP_DIR/tasks-app.backup"
        log_info "Backed up binary to $BACKUP_DIR/tasks-app.backup"
        return 0
    fi

    return 1
}

restore_binary() {
    local backup_path="$BACKUP_DIR/tasks-app.backup"
    local binary_path="target/release/tasks-app"

    if [[ -f "$backup_path" ]]; then
        cp "$backup_path" "$binary_path"
        log_warn "Restored binary from backup"
        return 0
    fi

    log_error "No backup binary available"
    return 1
}

# Rebuild based on scope
rebuild() {
    local scope="$1"
    local build_failed=0

    log_info "Rebuilding with scope: $scope"
    write_state "building:$scope"

    # Backup current binary before rebuild
    backup_binary || true

    case "$scope" in
        none)
            log_info "No rebuild needed"
            ;;
        frontend)
            log_info "Rebuilding frontend..."
            write_state "building:frontend"
            if command -v bun &> /dev/null; then
                if ! (cd web && bun install && bun run build); then
                    log_error "Frontend build failed"
                    build_failed=1
                fi
            else
                log_warn "bun not found, skipping frontend rebuild"
            fi
            ;;
        server)
            log_info "Rebuilding server..."
            write_state "building:server"
            if ! cargo build --release --package tasks-app; then
                log_error "Server build failed"
                build_failed=1
            fi
            ;;
        container)
            log_info "Rebuilding container image..."
            write_state "building:container"
            if ! make container-image; then
                log_error "Container image build failed"
                build_failed=1
            fi
            ;;
        server_and_frontend)
            log_info "Rebuilding server and frontend..."
            write_state "building:server"
            if ! cargo build --release --package tasks-app; then
                log_error "Server build failed"
                build_failed=1
            fi
            if [[ $build_failed -eq 0 ]]; then
                write_state "building:frontend"
                if command -v bun &> /dev/null; then
                    if ! (cd web && bun install && bun run build); then
                        log_error "Frontend build failed"
                        build_failed=1
                    fi
                else
                    log_warn "bun not found, skipping frontend rebuild"
                fi
            fi
            ;;
        server_and_container)
            log_info "Rebuilding server and container..."
            write_state "building:server"
            if ! cargo build --release --package tasks-app; then
                log_error "Server build failed"
                build_failed=1
            fi
            if [[ $build_failed -eq 0 ]]; then
                write_state "building:container"
                if ! make container-image; then
                    log_error "Container image build failed"
                    build_failed=1
                fi
            fi
            ;;
        all)
            log_info "Full rebuild..."
            write_state "building:server"
            if ! cargo build --release --package tasks-app; then
                log_error "Server build failed"
                build_failed=1
            fi
            if [[ $build_failed -eq 0 ]]; then
                write_state "building:frontend"
                if command -v bun &> /dev/null; then
                    if ! (cd web && bun install && bun run build); then
                        log_error "Frontend build failed"
                        build_failed=1
                    fi
                else
                    log_warn "bun not found, skipping frontend rebuild"
                fi
            fi
            if [[ $build_failed -eq 0 ]]; then
                write_state "building:container"
                if ! make container-image; then
                    log_error "Container image build failed"
                    build_failed=1
                fi
            fi
            ;;
        *)
            log_warn "Unknown scope '$scope', performing server rebuild"
            write_state "building:server"
            if ! cargo build --release --package tasks-app; then
                log_error "Server build failed"
                build_failed=1
            fi
            ;;
    esac

    if [[ $build_failed -eq 1 ]]; then
        log_error "Build failed, attempting to restore backup"
        if restore_binary; then
            log_warn "Using backup binary, will retry update on next check"
        else
            log_error "Cannot restore backup, server may fail to start"
        fi
        return 1
    fi

    log_success "Rebuild complete"
    return 0
}

# Resume partial update
resume_update() {
    local state
    state=$(read_state)

    case "$state" in
        none)
            return 1
            ;;
        fetching|merging)
            log_info "Resuming update from state: $state (restarting fetch)"
            if pull_updates; then
                local scope
                scope=$(read_scope)
                rebuild "$scope" && cleanup_state
            fi
            return 0
            ;;
        building:*)
            local scope="${state#building:}"
            log_info "Resuming build from state: $scope"
            if rebuild "$scope"; then
                cleanup_state
            fi
            return $?
            ;;
        *)
            log_warn "Unknown state: $state, cleaning up"
            cleanup_state
            return 1
            ;;
    esac
}

# Health check after restart
health_check() {
    local port="${TASKS_WEB_PORT:-4800}"
    local url="http://localhost:$port/api/mode"
    local attempts=0
    local max_attempts=$((HEALTH_TIMEOUT / 2))

    log_info "Waiting for server to become healthy..."

    while [[ $attempts -lt $max_attempts ]]; do
        if curl -sf "$url" > /dev/null 2>&1; then
            log_success "Server is healthy"
            return 0
        fi
        sleep 2
        attempts=$((attempts + 1))
    done

    log_warn "Health check timed out after ${HEALTH_TIMEOUT}s"
    return 1
}

# Main loop
main() {
    log_info "Starting Tasks server runner"
    log_info "Data directory: $DATA_DIR"

    # Rotate log if needed
    rotate_log

    # Ensure directories exist
    mkdir -p "$DATA_DIR"
    mkdir -p "$BACKUP_DIR"

    # Check for stale PID
    check_stale_pid

    # Write our PID
    write_pid

    # Setup signal handlers
    setup_signal_handlers

    # Check for interrupted update to resume
    if resume_update; then
        log_info "Resumed from interrupted update"
    fi

    while true; do
        log_info "Starting server..."
        rotate_log

        # Run the server in background so we can track PID
        set +e
        if [[ -x ./target/release/tasks-app ]]; then
            ./target/release/tasks-app run "$@" &
        else
            cargo run --release --package tasks-app -- run "$@" &
        fi
        SERVER_PID=$!

        # Run health check in background (non-blocking, just logging)
        health_check &

        # Wait for server to exit
        wait $SERVER_PID
        exit_code=$?
        SERVER_PID=""
        set -e

        if [[ $exit_code -eq $UPDATE_EXIT_CODE ]]; then
            log_info "Server requested update restart (exit code $UPDATE_EXIT_CODE)"

            # Read the scope before cleaning up
            scope=$(read_scope)

            # Pull and rebuild
            if pull_updates && rebuild "$scope"; then
                cleanup_state
                log_success "Update complete, restarting server..."

                # Small delay before restart
                sleep 2
            else
                cleanup_state
                log_error "Update failed, restarting server with current version..."
                sleep 2
            fi
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
    exit ${exit_code:-0}
}

# Run main with all arguments
main "$@"
