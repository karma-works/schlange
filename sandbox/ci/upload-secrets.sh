#!/usr/bin/env bash
# Lädt die Sandbox-Zugangsdaten als Actions-Secrets ins Schlange-Repo (Workflow sandbox.yml).
#
#   bash sandbox/ci/upload-secrets.sh            # alle Secrets setzen bzw. ersetzen
#   bash sandbox/ci/upload-secrets.sh --list     # nur anzeigen, welche gesetzt sind
#
# Quelle: ~/.config/schlange-sandbox/. Werte gehen über stdin an gh, nie über die Kommandozeile.
set -euo pipefail
REPO="${SCHLANGE_REPO:-karma-works/schlange}"
CONFIG="${SCHLANGE_SANDBOX_CONFIG:-$HOME/.config/schlange-sandbox}"

die() { echo "Fehler: $*" >&2; exit 1; }

if [ "${1:-}" = --list ]; then
  gh secret list -R "$REPO" | grep '^SANDBOX_' || echo "keine SANDBOX_-Secrets"
  gh variable list -R "$REPO" | grep '^SANDBOX_' || true
  exit 0
fi

# Secret-Name → Datei unter $CONFIG
pairs="SANDBOX_QUEUE_APP=apps/queue.json
SANDBOX_QUEUE_PEM=apps/queue.pem
SANDBOX_QUEUE_WEBHOOK_SECRET=apps/queue.webhook-secret
SANDBOX_DRIVER_APP=apps/driver.json
SANDBOX_DRIVER_PEM=apps/driver.pem
SANDBOX_HUMAN_TOKEN=human-token
SANDBOX_MERGIFY_API_KEY=mergify-api-key"

for pair in $pairs; do
  [ -s "$CONFIG/${pair#*=}" ] || die "fehlt: $CONFIG/${pair#*=}"
done
for pair in $pairs; do
  gh secret set "${pair%%=*}" -R "$REPO" < "$CONFIG/${pair#*=}"
  echo "→ ${pair%%=*} gesetzt"
done
# shellcheck disable=SC1091
. "$CONFIG/sandbox.env"
gh variable set SANDBOX_SMEE_URL -R "$REPO" --body "$SMEE_URL" >/dev/null
echo "→ Variable SANDBOX_SMEE_URL gesetzt"
