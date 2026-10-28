#!/usr/bin/env bash
# Im Actions-Runner: baut ~/.config/schlange-sandbox aus den Secrets nach (Gegenstück zu
# upload-secrets.sh), damit Driver und setup.sh dort wie lokal laufen.
set -euo pipefail
umask 077
CONFIG="${RUNNER_TEMP:?nur im Runner}/schlange-sandbox"
mkdir -p "$CONFIG/apps"
printf '%s\n' "$SANDBOX_QUEUE_APP" > "$CONFIG/apps/queue.json"
printf '%s\n' "$SANDBOX_QUEUE_PEM" > "$CONFIG/apps/queue.pem"
printf '%s' "$SANDBOX_QUEUE_WEBHOOK_SECRET" > "$CONFIG/apps/queue.webhook-secret"
printf '%s\n' "$SANDBOX_DRIVER_APP" > "$CONFIG/apps/driver.json"
printf '%s\n' "$SANDBOX_DRIVER_PEM" > "$CONFIG/apps/driver.pem"
printf 'SMEE_URL=%q\n' "${SANDBOX_SMEE_URL:-}" > "$CONFIG/sandbox.env"
echo "SCHLANGE_SANDBOX_CONFIG=$CONFIG" >> "$GITHUB_ENV"
echo "SANDBOX_RUNS=$RUNNER_TEMP/sandbox-runs" >> "$GITHUB_ENV"
echo "Zugangsdaten unter $CONFIG"
