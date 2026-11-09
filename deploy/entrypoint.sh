#!/bin/sh
# Restores ~/.eraser from R2 (via the Worker), runs Eraser, and syncs state back.
set -eu
STATE_URL="http://state.internal/"
HOME_DIR="$HOME/.eraser"
mkdir -p "$HOME_DIR" && chmod 700 "$HOME_DIR"

if curl -fsS "$STATE_URL" -o /tmp/state.tar.gz; then
  tar -xzf /tmp/state.tar.gz -C "$HOME" && echo "state restored"
else
  echo "no saved state, starting fresh"
fi

sync_state() {
  # sqlite3 .backup gives a consistent copy even while the DB is open
  rm -rf /tmp/snap && mkdir -p /tmp/snap/.eraser
  for f in "$HOME_DIR"/*; do
    [ -e "$f" ] || continue
    case "$f" in
      *.db) sqlite3 "$f" ".backup /tmp/snap/.eraser/$(basename "$f")" ;;
      *-wal|*-shm) ;;
      *) cp -p "$f" /tmp/snap/.eraser/ ;;
    esac
  done
  tar -czf /tmp/snap.tar.gz -C /tmp/snap .eraser
  NEW=$(sha256sum /tmp/snap.tar.gz | cut -d' ' -f1)
  if [ "$NEW" != "${LAST:-}" ]; then
    curl -fsS -X PUT --data-binary @/tmp/snap.tar.gz "$STATE_URL" && LAST=$NEW
  fi
}

eraser serve --host 0.0.0.0 --port 8080 --public-origin "https://erase.kampa.app" &
APP=$!

shutdown() {
  kill -TERM "$APP" 2>/dev/null || true
  wait "$APP" 2>/dev/null || true
  sync_state
  exit 0
}
trap shutdown TERM INT

# ponytail: 60s snapshot interval, so at most a minute of history is lost on a hard kill
while kill -0 "$APP" 2>/dev/null; do
  sleep 60 & wait $!
  sync_state || echo "state sync failed"
done
sync_state
