#!/usr/bin/env bash
# tasks-runner.sh - Wrapper script for Tasks server with self-update support
#
# This script manages the Tasks server lifecycle and handles self-updates.
# When the server exits with code 100, it triggers an update sequence.
#
# Features:
# - Build failure handling with fallback to previous binary
# - Network failure handling with exponential backoff
# - Partial update recovery
# - Signal handling (SIGTERM, SIGINT)
# - PID file management
# - Logging to file
# - Health check integration
#
# Usage:
#   ./scripts/tasks-runner.sh [--web]
#
# Environment:
#   TASKS_DATA_DIR          Data directory (default: ~/.local/state/tasks)
#   TASKS_RUNNER_LOG        Log file path (default: $DATA_DIR/runner.log)
#   TASKS_HEALTH_TIMEOUT    Health check timeout in seconds (default: 30)
#   TASKS_HEALTH_ENDPOINT   Health check URL (default: http://localhost:4800/api/mode)
#   TASKS_MAX_RETRIES       Max git fetch retries (default: 5)
#   TASKS_RETRY_DELAY       Initial retry delay in seconds (default: 5)

set -euo pipefail

# -----------------------------------------------------------------------------
# Configuration
# -----------------------------------------------------------------------------

DATA_DIR="${TASKS_DATA_DIR:-$HOME/.local/state/tasks}"
LOG_FILE="${TASKS_RUNNER_LOG:-$DATA_DIR/runner.log}"
PID_FILE="${TASKS_PID_FILE:-$DATA_DIR/tasks.pid}"
STATE_FILE="${DATA_DIR}/.update-state"
SCOPE_FILE="${DATA_DIR}/.update-scope"
HEALTH_TIMEOUT="${TASKS_HEALTH_TIMEOUT:-30}"
HEALTH_ENDPOINT="${TASKS_HEALTH_ENDPOINT:-http://localhost:4800/api/mode}"
MAX_RETRIES="${TASKS_MAX_RETRIES:-5}"
INITIAL_RETRY_DELAY="${TASKS_RETRY_DELAY:-5}"

# Resolve paths relative to script location
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY_PATH="${PROJECT_ROOT}/target/release/tasks"
BACKUP_BINARY_PATH="${PROJECT_ROOT}/target/release/tasks.backup"

# Track child process
SERVER_PID=""
SHUTDOWN_REQUESTED=false

# Network failure tracking (for backoff)
CONSECUTIVE_FAILURES=0
LAST_FAILURE_TIME=0

# -----------------------------------------------------------------------------
# Logging
# -----------------------------------------------------------------------------

log() {
    local level="$1"
    shift
    local timestamp
    timestamp="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "${timestamp} [${level}] $*" | tee -a "$LOG_FILE"
}

log_info() { log "INFO" "$@"; }
log_warn() { log "WARN" "$@"; }
log_error() { log "ERROR" "$@"; }

rotate_log() {
    local max_size=$((10 * 1024 * 1024))  # 10MB
    if [[ -f "$LOG_FILE" ]] && [[ $(stat -f%z "$LOG_FILE" 2>/dev/null || stat -c%s "$LOG_FILE" 2>/dev/null) -gt $max_size ]]; then
        mv "$LOG_FILE" "${LOG_FILE}.1"
        log_info "Log rotated"
    fi
}

# -----------------------------------------------------------------------------
# PID File Management
# -----------------------------------------------------------------------------

write_pid() {
    echo $$ > "$PID_FILE"
    log_info "PID file written: $PID_FILE ($$)"
}

cleanup_pid() {
    if [[ -f "$PID_FILE" ]]; then
        rm -f "$PID_FILE"
        log_info "PID file removed"
    fi
}

check_already_running() {
    if [[ -f "$PID_FILE" ]]; then
        local old_pid
        old_pid=$(cat "$PID_FILE")
        if kill -0 "$old_pid" 2>/dev/null; then
            log_error "Another instance is already running (PID: $old_pid)"
            exit 1
        else
            log_warn "Stale PID file found, removing"
            rm -f "$PID_FILE"
        fi
    fi
}

# -----------------------------------------------------------------------------
# Signal Handling
# -----------------------------------------------------------------------------

shutdown_server() {
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        log_info "Forwarding signal to server (PID: $SERVER_PID)"
        kill -TERM "$SERVER_PID" 2>/dev/null || true

        # Wait for graceful shutdown with timeout
        local timeout=30
        local waited=0
        while kill -0 "$SERVER_PID" 2>/dev/null && [[ $waited -lt $timeout ]]; do
            sleep 1
            ((waited++))
        done

        if kill -0 "$SERVER_PID" 2>/dev/null; then
            log_warn "Server didn't stop gracefully, sending SIGKILL"
            kill -KILL "$SERVER_PID" 2>/dev/null || true
        fi
    fi
}

handle_signal() {
    local signal="$1"
    log_info "Received $signal"
    SHUTDOWN_REQUESTED=true
    shutdown_server
    cleanup_pid
    cleanup_state
    exit 0
}

trap 'handle_signal SIGTERM' SIGTERM
trap 'handle_signal SIGINT' SIGINT

# -----------------------------------------------------------------------------
# State Management (for partial update recovery)
# -----------------------------------------------------------------------------

# Update states:
# - FETCHING: Started git fetch
# - PULLING: Started git pull
# - BUILDING_CONTAINER: Building container image
# - BUILDING_SERVER: Building server binary
# - BUILDING_FRONTEND: Building frontend
# - RESTARTING: About to restart server
# - COMPLETE: Update finished successfully

write_state() {
    local state="$1"
    local extra="${2:-}"
    echo "${state}:${extra}" > "$STATE_FILE"
    log_info "Update state: $state ${extra:+($extra)}"
}

read_state() {
    if [[ -f "$STATE_FILE" ]]; then
        cat "$STATE_FILE"
    else
        echo ""
    fi
}

cleanup_state() {
    rm -f "$STATE_FILE" "$SCOPE_FILE"
}

get_state_name() {
    local state
    state=$(read_state)
    echo "${state%%:*}"
}

# -----------------------------------------------------------------------------
# Network Operations with Retry
# -----------------------------------------------------------------------------

calculate_backoff() {
    local attempt=$1
    local delay=$INITIAL_RETRY_DELAY
    for ((i = 1; i < attempt; i++)); do
        delay=$((delay * 2))
        if [[ $delay -gt 300 ]]; then
            delay=300  # Cap at 5 minutes
        fi
    done
    echo $delay
}

git_fetch_with_retry() {
    local attempt=1
    local now
    now=$(date +%s)

    # If we had a recent failure, wait before retrying
    if [[ $CONSECUTIVE_FAILURES -gt 0 ]]; then
        local time_since_failure=$((now - LAST_FAILURE_TIME))
        local required_delay
        required_delay=$(calculate_backoff $CONSECUTIVE_FAILURES)

        if [[ $time_since_failure -lt $required_delay ]]; then
            local remaining=$((required_delay - time_since_failure))
            log_info "Backing off for ${remaining}s due to previous failures"
            sleep $remaining
        fi
    fi

    while [[ $attempt -le $MAX_RETRIES ]]; do
        log_info "Fetching from origin (attempt $attempt/$MAX_RETRIES)"

        if git -C "$PROJECT_ROOT" fetch origin main 2>&1 | tee -a "$LOG_FILE"; then
            CONSECUTIVE_FAILURES=0
            return 0
        fi

        LAST_FAILURE_TIME=$(date +%s)
        CONSECUTIVE_FAILURES=$((CONSECUTIVE_FAILURES + 1))

        if [[ $attempt -lt $MAX_RETRIES ]]; then
            local delay
            delay=$(calculate_backoff $attempt)
            log_warn "Git fetch failed, retrying in ${delay}s"
            sleep $delay
        fi

        ((attempt++))
    done

    log_error "Git fetch failed after $MAX_RETRIES attempts"
    return 1
}

git_pull_with_retry() {
    local attempt=1

    while [[ $attempt -le $MAX_RETRIES ]]; do
        log_info "Pulling from origin/main (attempt $attempt/$MAX_RETRIES)"

        if git -C "$PROJECT_ROOT" pull origin main 2>&1 | tee -a "$LOG_FILE"; then
            return 0
        fi

        if [[ $attempt -lt $MAX_RETRIES ]]; then
            local delay
            delay=$(calculate_backoff $attempt)
            log_warn "Git pull failed, retrying in ${delay}s"
            sleep $delay
        fi

        ((attempt++))
    done

    log_error "Git pull failed after $MAX_RETRIES attempts"
    return 1
}

# -----------------------------------------------------------------------------
# Build Operations
# -----------------------------------------------------------------------------

backup_binary() {
    if [[ -f "$BINARY_PATH" ]]; then
        log_info "Backing up current binary"
        cp "$BINARY_PATH" "$BACKUP_BINARY_PATH"
    fi
}

restore_binary() {
    if [[ -f "$BACKUP_BINARY_PATH" ]]; then
        log_warn "Restoring previous binary"
        cp "$BACKUP_BINARY_PATH" "$BINARY_PATH"
        return 0
    else
        log_error "No backup binary available"
        return 1
    fi
}

build_container() {
    log_info "Building container image..."
    write_state "BUILDING_CONTAINER"

    if make -C "$PROJECT_ROOT" container-image 2>&1 | tee -a "$LOG_FILE"; then
        log_info "Container image built successfully"
        return 0
    else
        log_error "Container image build failed"
        return 1
    fi
}

build_server() {
    log_info "Building server binary..."
    write_state "BUILDING_SERVER"

    backup_binary

    if cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1 | tee -a "$LOG_FILE"; then
        log_info "Server binary built successfully"
        return 0
    else
        log_error "Server binary build failed"
        restore_binary
        return 1
    fi
}

build_frontend() {
    log_info "Building frontend..."
    write_state "BUILDING_FRONTEND"

    local web_dir="$PROJECT_ROOT/web"
    if [[ -d "$web_dir" ]]; then
        if (cd "$web_dir" && bun install && bun run build) 2>&1 | tee -a "$LOG_FILE"; then
            log_info "Frontend built successfully"
            return 0
        else
            log_error "Frontend build failed"
            return 1
        fi
    else
        log_warn "Web directory not found, skipping frontend build"
        return 0
    fi
}

# -----------------------------------------------------------------------------
# Update Sequence
# -----------------------------------------------------------------------------

read_scope() {
    if [[ -f "$SCOPE_FILE" ]]; then
        cat "$SCOPE_FILE"
    else
        echo "server"  # Default to server rebuild
    fi
}

perform_update() {
    local state
    state=$(get_state_name)
    local scope
    scope=$(read_scope)

    log_info "=== Starting update sequence ==="
    log_info "Scope: $scope, Resuming from: ${state:-START}"

    # Resume from last successful state if recovering
    case "$state" in
        ""|"FETCHING")
            write_state "PULLING"
            if ! git_pull_with_retry; then
                log_error "Update failed at PULLING stage"
                cleanup_state
                return 1
            fi
            ;&  # Fall through

        "PULLING")
            if [[ "$scope" == "container" ]]; then
                if ! build_container; then
                    log_error "Update failed at BUILDING_CONTAINER stage"
                    cleanup_state
                    return 1
                fi
            fi
            ;&  # Fall through

        "BUILDING_CONTAINER")
            if [[ "$scope" == "container" || "$scope" == "server" ]]; then
                if ! build_server; then
                    log_error "Update failed at BUILDING_SERVER stage"
                    cleanup_state
                    return 1
                fi
            fi
            ;&  # Fall through

        "BUILDING_SERVER")
            if ! build_frontend; then
                log_error "Update failed at BUILDING_FRONTEND stage"
                # Frontend failures are non-fatal for server operation
                log_warn "Continuing despite frontend build failure"
            fi
            ;;

        "BUILDING_FRONTEND"|"RESTARTING")
            # Already past build stages, just restart
            log_info "Resuming from near completion"
            ;;

        *)
            log_warn "Unknown state: $state, starting fresh"
            ;;
    esac

    write_state "COMPLETE"
    cleanup_state
    log_info "=== Update sequence complete ==="
    return 0
}

# -----------------------------------------------------------------------------
# Health Check
# -----------------------------------------------------------------------------

wait_for_health() {
    log_info "Waiting for server health check..."
    local timeout=$HEALTH_TIMEOUT
    local waited=0

    while [[ $waited -lt $timeout ]]; do
        if curl -sf "$HEALTH_ENDPOINT" > /dev/null 2>&1; then
            log_info "Server is healthy"
            return 0
        fi
        sleep 1
        ((waited++))
    done

    log_warn "Health check timed out after ${timeout}s"
    return 1
}

# -----------------------------------------------------------------------------
# Main Loop
# -----------------------------------------------------------------------------

ensure_data_dir() {
    mkdir -p "$DATA_DIR"
}

run_server() {
    local args=("$@")

    if [[ ! -x "$BINARY_PATH" ]]; then
        log_error "Server binary not found: $BINARY_PATH"
        log_info "Building server..."
        if ! build_server; then
            log_error "Failed to build server"
            return 1
        fi
    fi

    log_info "Starting server: $BINARY_PATH run ${args[*]:-}"
    write_state "RESTARTING"

    # Start server in background
    "$BINARY_PATH" run "${args[@]}" &
    SERVER_PID=$!

    log_info "Server started (PID: $SERVER_PID)"

    # Wait for health if --web flag is present
    if [[ " ${args[*]} " =~ " --web " ]]; then
        sleep 2  # Give server time to bind
        wait_for_health || log_warn "Server may not be fully ready"
    fi

    cleanup_state

    # Wait for server to exit
    wait $SERVER_PID
    return $?
}

main() {
    rotate_log
    ensure_data_dir
    check_already_running
    write_pid

    log_info "=== Tasks Runner starting ==="
    log_info "Project root: $PROJECT_ROOT"
    log_info "Data directory: $DATA_DIR"
    log_info "Log file: $LOG_FILE"

    # Check for interrupted update
    local state
    state=$(get_state_name)
    if [[ -n "$state" && "$state" != "COMPLETE" ]]; then
        log_warn "Found interrupted update in state: $state"
        log_info "Attempting to resume update..."
        if ! perform_update; then
            log_error "Failed to complete interrupted update"
            log_info "Attempting to start with existing binary"
        fi
    fi

    # Pass through command line arguments
    local server_args=("$@")

    while true; do
        if [[ "$SHUTDOWN_REQUESTED" == "true" ]]; then
            log_info "Shutdown requested, exiting"
            break
        fi

        run_server "${server_args[@]}" || true
        exit_code=$?
        SERVER_PID=""

        log_info "Server exited with code: $exit_code"

        case $exit_code in
            100)
                # Update requested
                log_info "Update requested (exit code 100)"
                if perform_update; then
                    log_info "Update successful, restarting server"
                else
                    log_error "Update failed"
                    if [[ -f "$BACKUP_BINARY_PATH" ]]; then
                        log_warn "Falling back to previous binary"
                        restore_binary
                    else
                        log_error "No fallback available, waiting before retry"
                        sleep 30
                    fi
                fi
                ;;
            0)
                # Clean shutdown
                log_info "Clean shutdown"
                break
                ;;
            *)
                # Unexpected exit
                log_error "Unexpected exit, not restarting"
                break
                ;;
        esac
    done

    cleanup_pid
    log_info "=== Tasks Runner stopped ==="
}

main "$@"
