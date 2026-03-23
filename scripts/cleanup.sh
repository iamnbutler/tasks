#!/usr/bin/env bash
# Clean up local tasks state: wipe DB, events, and remove all task containers.
set -euo pipefail

DATA_DIR="${TASKS_DATA_DIR:-$HOME/.local/state/tasks}"

echo "==> Stopping and removing task-agent containers..."
container list 2>/dev/null | grep tasks-agent | awk '{print $1}' | while read -r id; do
  container stop "$id" 2>/dev/null && container rm "$id" 2>/dev/null && echo "    removed $id"
done || true
echo "    done"

echo "==> Wiping local database and events..."
rm -f "$DATA_DIR/db.sqlite" "$DATA_DIR/db.sqlite-shm" "$DATA_DIR/db.sqlite-wal"
rm -rf "$DATA_DIR/events"
rm -f "$DATA_DIR/server.log"
echo "    removed $DATA_DIR/{db.sqlite,events/,server.log}"

echo "==> Cleanup complete"
