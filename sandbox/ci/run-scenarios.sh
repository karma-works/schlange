#!/usr/bin/env bash
# Spielt Szenarien nacheinander ab und vergleicht mit dem Sollwert, falls vorhanden.
#
#   bash sandbox/ci/run-scenarios.sh <repo> all
#   bash sandbox/ci/run-scenarios.sh <repo> s01-einzelner-pr s03-rotes-buendel
#
# Sollwert: sandbox/baselines/<szenario>.json (aus einem geprüften Mergify-Lauf, ohne
# Mergify-Texte). Auf queue-lab erkennt der Vergleich, wenn Mergify sein Verhalten ändert;
# auf schlange-e2e ist er der Kompatibilitätstest von Schlange.
# Exit 1, wenn ein Szenario abweicht oder scheitert. Ergebnis zusätzlich in GITHUB_STEP_SUMMARY.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
repo="${1:?repo}"; shift
if [ "${1:-all}" = all ]; then
  set --
  for f in "$HERE"/scenarios/*.json; do set -- "$@" "$(basename "$f" .json)"; done
fi

if [ "$repo" = schlange-e2e ] && ! command -v schlange >/dev/null; then
  echo "::error::Für schlange-e2e muss schlange serve mit der Queue-App laufen (noch nicht in diesem Workflow eingebaut)."
  exit 1
fi

summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
{ echo "### Sandbox $repo"; echo; echo "| Szenario | Ergebnis |"; echo "|---|---|"; } >> "$summary"
failed=0
for s in "$@"; do
  args=(run "$HERE/scenarios/$s.json" --repo "$repo")
  base="$HERE/baselines/$s.json"
  [ -f "$base" ] && args+=(--baseline "$base")
  echo "::group::$s"
  python3 -I "$HERE/driver/sandbox.py" "${args[@]}"
  rc=$?
  echo "::endgroup::"
  if [ $rc -eq 0 ]; then
    result=$([ -f "$base" ] && echo "gleich wie Sollwert" || echo "aufgezeichnet (kein Sollwert)")
  else
    result="**Abweichung oder Fehler** (Exit $rc)"; failed=1
  fi
  echo "| $s | $result |" >> "$summary"
  echo "$s: $result"
done
exit $failed
