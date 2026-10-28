#!/usr/bin/env bash
# In the Actions runner: starts `schlange serve` against schlange-sandbox/schlange-e2e with
# the sandbox queue app (credentials written by write-config.sh), in the background.
#
#   bash sandbox/ci/start-schlange.sh [path of the schlange binary]
#
# Webhooks: when SANDBOX_SMEE_URL is set, smee-client forwards the channel to the local
# listener. Without smee (or when a delivery is lost) the reconciliation loop (every
# RECONCILE seconds) finds the work by polling, so the run works either way, only slower.
# Writes SCHLANGE_URL to GITHUB_ENV and the process ids to $RUNNER_TEMP/schlange.pids.
# Only the schlange-sandbox organisation and the sandbox credentials are ever used.
set -euo pipefail
umask 077
BIN="${1:-target/release/schlange}"
CONFIG="${SCHLANGE_SANDBOX_CONFIG:?run write-config.sh first}"
TMP="${RUNNER_TEMP:?only in the runner}"
REPO="schlange-sandbox/schlange-e2e"
PORT="${SCHLANGE_PORT:-8080}"
RECONCILE="${RECONCILE:-20}"

[ -x "$BIN" ] || { echo "::error::no schlange binary at $BIN"; exit 1; }
[ -s ".research/mergify/schema.json" ] || { echo "::error::no Mergify schema (tools/fetch-mergify-sources)"; exit 1; }

app_id="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$CONFIG/apps/queue.json")"
mkdir -p "$TMP/schlange-data"

nohup "$BIN" serve \
  --app-id "$app_id" \
  --private-key "$CONFIG/apps/queue.pem" \
  --webhook-secret-file "$CONFIG/apps/queue.webhook-secret" \
  --repo "$REPO" \
  --data-dir "$TMP/schlange-data" \
  --schema .research/mergify/schema.json \
  --listen "127.0.0.1:$PORT" \
  --reconcile-every "$RECONCILE" \
  --no-lease \
  >"$TMP/schlange-serve.log" 2>&1 &
echo "$!" > "$TMP/schlange.pids"

if [ -n "${SANDBOX_SMEE_URL:-}" ]; then
  # Pinned version; the channel URL is a variable of the repository, not a secret.
  nohup npx --yes smee-client@2.0.4 -u "$SANDBOX_SMEE_URL" -t "http://127.0.0.1:$PORT/webhook" \
    >"$TMP/smee.log" 2>&1 &
  echo "$!" >> "$TMP/schlange.pids"
else
  echo "::notice::no SANDBOX_SMEE_URL: Schlange polls (reconcile every ${RECONCILE}s)"
fi

for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "schlange serve is up ($REPO, reconcile every ${RECONCILE}s)"
    echo "SCHLANGE_URL=http://127.0.0.1:$PORT" >> "${GITHUB_ENV:-/dev/null}"
    exit 0
  fi
  if ! kill -0 "$(head -1 "$TMP/schlange.pids")" 2>/dev/null; then
    echo "::error::schlange serve exited"; tail -n 40 "$TMP/schlange-serve.log"; exit 1
  fi
  sleep 1
done
echo "::error::schlange serve did not answer /healthz"; tail -n 40 "$TMP/schlange-serve.log"; exit 1
