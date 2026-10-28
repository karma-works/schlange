#!/usr/bin/env bash
# Reset tracked files on schlange-e2e main to the fixture after the repository is idle.
set -euo pipefail
[ "${SANDBOX_ORG:-schlange-sandbox}" = schlange-sandbox ] || { echo 'refused: only schlange-sandbox' >&2; exit 1; }
[ "${1:-}" = schlange-e2e ] || { echo 'refused: fixture update is limited to schlange-e2e' >&2; exit 1; }
HERE="$(cd "$(dirname "$0")/.." && pwd)"
python3 -I "$HERE/driver/sandbox.py" wait-idle --repo schlange-e2e --all-open
bash "$HERE/setup.sh" fixture-e2e
