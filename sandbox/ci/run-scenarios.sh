#!/usr/bin/env bash
# Replays scenarios one after another and compares them with the baseline, if there is one.
#
#   bash sandbox/ci/run-scenarios.sh <repo> all
#   bash sandbox/ci/run-scenarios.sh <repo> s01-single-pr s03-red-batch
#
# Baseline: sandbox/baselines/<scenario>.json (from a reviewed Mergify run, without
# Mergify texts). On queue-lab the comparison detects when Mergify changes its behaviour;
# on schlange-e2e it is Schlange's compatibility test.
# Exit 1 if a scenario deviates or fails. The result also goes to GITHUB_STEP_SUMMARY.
set -uo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
repo="${1:?repo}"; shift
if [ "${1:-all}" = all ]; then
  set --
  for f in "$HERE"/scenarios/*.json; do set -- "$@" "$(basename "$f" .json)"; done
fi

if [ "$repo" = schlange-e2e ] && ! command -v schlange >/dev/null; then
  echo "::error::schlange-e2e needs schlange serve running with the queue app (not built into this workflow yet)."
  exit 1
fi

summary="${GITHUB_STEP_SUMMARY:-/dev/null}"
{ echo "### Sandbox $repo"; echo; echo "| Scenario | Result |"; echo "|---|---|"; } >> "$summary"
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
    result=$([ -f "$base" ] && echo "same as baseline" || echo "recorded (no baseline)")
  else
    result="**Deviation or error** (exit $rc)"; failed=1
  fi
  echo "| $s | $result |" >> "$summary"
  echo "$s: $result"
done
exit $failed
