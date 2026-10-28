#!/usr/bin/env bash
# Explicit provisioning only; the setup command remains restricted to one disposable repository.
set -euo pipefail
[ "${SANDBOX_ORG:-schlange-sandbox}" = schlange-sandbox ] || { echo 'refused: only schlange-sandbox' >&2; exit 1; }
[ "${1:-}" = schlange-e2e-isolated ] || { echo 'refused: only the disposable repository' >&2; exit 1; }
[ "${3:-}" = false ] && [ "${4:-}" = false ] || { echo 'refused: provisioning cannot be combined with inspection or fixture reset' >&2; exit 1; }
case "${2:-}" in
  create) command=repos-isolated ;;
  resume) command=finish-isolated ;;
  *) echo 'refused: expected create or resume' >&2; exit 1 ;;
esac
HERE="$(cd "$(dirname "$0")/.." && pwd)"
bash "$HERE/setup.sh" "$command"
