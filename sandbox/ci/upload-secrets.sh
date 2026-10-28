#!/usr/bin/env bash
# Uploads the sandbox credentials as Actions secrets to the Schlange repo (workflow sandbox.yml).
#
#   bash sandbox/ci/upload-secrets.sh            # set or replace all secrets
#   bash sandbox/ci/upload-secrets.sh --list     # only show which ones are set
#
# Source: ~/.config/schlange-sandbox/. Values go to gh over stdin, never on the command line.
set -euo pipefail
REPO="${SCHLANGE_REPO:-karma-works/schlange}"
CONFIG="${SCHLANGE_SANDBOX_CONFIG:-$HOME/.config/schlange-sandbox}"

die() { echo "Error: $*" >&2; exit 1; }

if [ "${1:-}" = --list ]; then
  gh secret list -R "$REPO" | grep '^SANDBOX_' || echo "no SANDBOX_ secrets"
  gh variable list -R "$REPO" | grep '^SANDBOX_' || true
  exit 0
fi

# Secret name → file under $CONFIG
pairs="SANDBOX_QUEUE_APP=apps/queue.json
SANDBOX_QUEUE_PEM=apps/queue.pem
SANDBOX_QUEUE_WEBHOOK_SECRET=apps/queue.webhook-secret
SANDBOX_DRIVER_APP=apps/driver.json
SANDBOX_DRIVER_PEM=apps/driver.pem
SANDBOX_HUMAN_TOKEN=human-token
SANDBOX_MERGIFY_API_KEY=mergify-api-key"

for pair in $pairs; do
  [ -s "$CONFIG/${pair#*=}" ] || die "missing: ${CONFIG}/${pair#*=}"
done
for pair in $pairs; do
  gh secret set "${pair%%=*}" -R "$REPO" < "$CONFIG/${pair#*=}"
  echo "→ ${pair%%=*} set"
done
# shellcheck disable=SC1091
. "$CONFIG/sandbox.env"
gh variable set SANDBOX_SMEE_URL -R "$REPO" --body "$SMEE_URL" >/dev/null
echo "→ Variable SANDBOX_SMEE_URL set"
