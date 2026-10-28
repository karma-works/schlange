#!/usr/bin/env bash
# In the Actions runner: rebuilds ~/.config/schlange-sandbox from the secrets (counterpart of
# upload-secrets.sh), so the driver and setup.sh run there as they do locally.
set -euo pipefail
umask 077
CONFIG="${RUNNER_TEMP:?only in the runner}/schlange-sandbox"
mkdir -p "$CONFIG/apps"
printf '%s\n' "$SANDBOX_QUEUE_APP" > "$CONFIG/apps/queue.json"
printf '%s\n' "$SANDBOX_QUEUE_PEM" > "$CONFIG/apps/queue.pem"
printf '%s' "$SANDBOX_QUEUE_WEBHOOK_SECRET" > "$CONFIG/apps/queue.webhook-secret"
printf '%s\n' "$SANDBOX_DRIVER_APP" > "$CONFIG/apps/driver.json"
printf '%s\n' "$SANDBOX_DRIVER_PEM" > "$CONFIG/apps/driver.pem"
printf 'SMEE_URL=%q\n' "${SANDBOX_SMEE_URL:-}" > "$CONFIG/sandbox.env"
echo "SCHLANGE_SANDBOX_CONFIG=$CONFIG" >> "$GITHUB_ENV"
echo "SANDBOX_RUNS=$RUNNER_TEMP/sandbox-runs" >> "$GITHUB_ENV"
echo "Credentials under $CONFIG"
