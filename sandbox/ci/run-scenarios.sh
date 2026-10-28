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
case "$repo" in queue-lab|schlange-e2e) ;; *) echo "::error::refused: $repo is not a sandbox repository"; exit 1;; esac
[ "${SANDBOX_ORG:-schlange-sandbox}" = schlange-sandbox ] || { echo "::error::refused: only the organisation schlange-sandbox"; exit 1; }
if [ "${1:-all}" = all ]; then
  set --
  for f in "$HERE"/scenarios/*.json; do set -- "$@" "$(basename "$f" .json)"; done
fi

# schlange-e2e is Schlange's compatibility test: `schlange serve` must be running
# (sandbox/ci/start-schlange.sh in the workflow, or by hand with SCHLANGE_URL set).
if [ "$repo" = schlange-e2e ] && ! curl -fsS "${SCHLANGE_URL:-http://127.0.0.1:8080}/healthz" >/dev/null 2>&1; then
  echo "::error::schlange-e2e needs schlange serve running with the queue app (sandbox/ci/start-schlange.sh)."
  exit 1
fi

# On schlange-e2e a wait that times out is a failure, with or without a baseline.
[ "$repo" = schlange-e2e ] && export SANDBOX_FAIL_ON_TIMEOUT=1

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
