#!/usr/bin/env bash
# Reset tracked files on one sandbox main branch to the fixture after it is idle.
set -euo pipefail
[ "${SANDBOX_ORG:-schlange-sandbox}" = schlange-sandbox ] || { echo 'refused: only schlange-sandbox' >&2; exit 1; }
case "${1:-}" in
  schlange-e2e) setup=fixture-e2e ;;
  queue-lab) setup=fixture-lab ;;
  *) echo 'refused: unknown sandbox repository' >&2; exit 1 ;;
esac
HERE="$(cd "$(dirname "$0")/.." && pwd)"
python3 -I "$HERE/driver/sandbox.py" wait-idle --repo "$1" --all-open
bash "$HERE/setup.sh" "$setup"
