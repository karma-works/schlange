#!/usr/bin/env bash
# Measures the size of the container image and compares it with image-budget.txt (bytes).
#   scripts/image-size.sh [image]     default image: schlange:dev
# Exit 0 within budget, 1 over budget, 2 when the image does not exist.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
image="${1:-schlange:dev}"
budget="$(grep -v '^#' "$root/image-budget.txt" | tr -d '[:space:]')"

if ! size="$(docker image inspect --format '{{.Size}}' "$image" 2>/dev/null)"; then
  echo "image $image not found; build it with: docker build -t $image ." >&2
  exit 2
fi

mib() { awk -v b="$1" 'BEGIN { printf "%.2f MiB", b / 1048576 }'; }
echo "image $image: $size bytes ($(mib "$size")), budget $budget bytes ($(mib "$budget"))"
if [ "$size" -gt "$budget" ]; then
  echo "over budget by $((size - budget)) bytes" >&2
  exit 1
fi
echo "within budget"
