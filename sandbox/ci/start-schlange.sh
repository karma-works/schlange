#!/usr/bin/env bash
# In the Actions runner: starts `schlange serve` against an active sandbox, or read-only
# shadow observation on queue-lab, with the sandbox queue app, in the background.
#
#   bash sandbox/ci/start-schlange.sh [path of the schlange binary]
#
# No webhooks: the runner has no public address, and a forwarder from npm would run next to the
# queue app's key. The reconciliation loop (every RECONCILE seconds) polls instead, which is
# also what recovers lost webhooks in production.
# Writes SCHLANGE_URL to GITHUB_ENV and the process ids to $RUNNER_TEMP/schlange.pids.
# Only the schlange-sandbox organisation and the sandbox credentials are ever used.
set -euo pipefail
umask 077
[ "${SANDBOX_ORG:-schlange-sandbox}" = schlange-sandbox ] || { echo 'refused: only schlange-sandbox' >&2; exit 1; }
MODE="${SANDBOX_MODE:-active}"
case "${SANDBOX_REPO:-schlange-e2e}:$MODE" in
  schlange-e2e:active|schlange-e2e-isolated:active|queue-lab:shadow) ;;
  *) echo 'refused: only active E2E repositories or shadow on queue-lab' >&2; exit 1 ;;
esac
mode_args=()
if [ "$MODE" = shadow ]; then
  mode_args=(--shadow --mergify-login 'mergify[bot]' --mergify-app mergify)
fi
BIN="${1:-target/release/schlange}"
CONFIG="${SCHLANGE_SANDBOX_CONFIG:?run write-config.sh first}"
TMP="${RUNNER_TEMP:?only in the runner}"
REPO="schlange-sandbox/${SANDBOX_REPO:-schlange-e2e}"
PORT="${SCHLANGE_PORT:-8080}"
RECONCILE="${RECONCILE:-10}"

[ -x "$BIN" ] || { echo "::error::no schlange binary at $BIN"; exit 1; }
[ -s ".research/mergify/schema.json" ] || { echo "::error::no Mergify schema (tools/fetch-mergify-sources)"; exit 1; }

app_id="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$CONFIG/apps/queue.json")"
mkdir -p "$TMP/schlange-data"
# Admin API on 127.0.0.1 only, for the driver's event log (queued, enter_count, dequeue codes).
token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
echo "::add-mask::$token"
printf '%s' "$token" > "$TMP/admin-token"
echo "SCHLANGE_ADMIN_TOKEN=$token" >> "${GITHUB_ENV:-/dev/null}"

nohup "$BIN" serve "${mode_args[@]}" \
  --app-id "$app_id" \
  --private-key "$CONFIG/apps/queue.pem" \
  --webhook-secret-file "$CONFIG/apps/queue.webhook-secret" \
  --repo "$REPO" \
  --data-dir "$TMP/schlange-data" \
  --schema .research/mergify/schema.json \
  --listen "127.0.0.1:$PORT" \
  --reconcile-every "$RECONCILE" \
  --no-lease \
  --admin-token-file "$TMP/admin-token" \
  >"$TMP/schlange-serve.log" 2>&1 &
echo "$!" > "$TMP/schlange.pids"

for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:$PORT/readyz" >/dev/null 2>&1; then
    echo "schlange serve is ready ($REPO, $MODE, reconcile every ${RECONCILE}s)"
    echo "SCHLANGE_URL=http://127.0.0.1:$PORT" >> "${GITHUB_ENV:-/dev/null}"
    exit 0
  fi
  if ! kill -0 "$(head -1 "$TMP/schlange.pids")" 2>/dev/null; then
    echo "::error::schlange serve exited; the full log remains private in the runner"
    exit 1
  fi
  sleep 1
done
echo "::error::schlange serve did not become ready; the full log remains private in the runner"
exit 1
