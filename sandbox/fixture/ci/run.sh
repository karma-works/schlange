#!/usr/bin/env bash
# Controllable test suite of the sandbox.  run.sh <light|full> [BASE HEAD]
#
# Only flag files under ci/flags/ that changed between BASE and HEAD count (that is, those of
# the PR or of the batch in the queue PR). Without BASE/HEAD: no flags. One flag per line:
#   fail                 red in both modes
#   fail-light           red only in the PR (light)
#   fail-full            red only in the queue PR (full): "semantic conflict with main"
#   conflicts-with <id>  red in full mode if ci/flags/<id> is also in the batch
#   flaky <prozent>      red with this probability in full mode
#   sleep <sekunden>     run time, in both modes
#   sleep-full <sek>     run time, in full mode only
set -euo pipefail
mode="$1" base="${2:-}" head="${3:-}"
flags=()
if [ -n "$base" ] && [ -n "$head" ]; then
  while IFS= read -r f; do
    [ -f "$f" ] && flags+=("$f")
  done < <(git diff --name-only --no-renames "$base...$head" -- ci/flags/ | grep -v '/\.gitkeep$' || true)
fi
present() { local x; for x in "${flags[@]:-}"; do [ "$x" = "ci/flags/$1" ] && return 0; done; return 1; }
status=0
for f in "${flags[@]:-}"; do
  [ -n "$f" ] || continue
  echo "== $f"
  while read -r flag arg; do
    case "$flag" in
      ''|'#'*) ;;
      fail) echo "::error::$f: fail"; status=1 ;;
      fail-light) [ "$mode" = light ] && { echo "::error::$f: fail-light"; status=1; } ;;
      fail-full) [ "$mode" = full ] && { echo "::error::$f: fail-full"; status=1; } ;;
      conflicts-with) [ "$mode" = full ] && present "$arg" && { echo "::error::$f: conflict with $arg"; status=1; } ;;
      flaky) [ "$mode" = full ] && [ $((RANDOM % 100)) -lt "$arg" ] && { echo "::error::$f: flaky"; status=1; } ;;
      sleep) sleep "$arg" ;;
      sleep-full) [ "$mode" = full ] && sleep "$arg" ;;
      *) echo "::warning::$f: unknown flag $flag" ;;
    esac
  done < "$f"
done
[ "$status" = 0 ] && echo "Suite green ($mode)"
exit "$status"
