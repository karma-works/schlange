#!/usr/bin/env bash
# Steuerbare Test-Suite der Sandbox.  run.sh <light|full> [BASE HEAD]
#
# Es zählen nur Flag-Dateien unter ci/flags/, die zwischen BASE und HEAD geändert wurden
# (also vom PR bzw. vom Bündel im Queue-PR). Ohne BASE/HEAD: keine Flags. Je Zeile ein Flag:
#   fail                 rot in beiden Modi
#   fail-light           rot nur im PR (light)
#   fail-full            rot nur im Queue-PR (full): „semantischer Konflikt mit main“
#   conflicts-with <id>  rot im full-Modus, wenn auch ci/flags/<id> im Bündel steckt
#   flaky <prozent>      rot mit dieser Wahrscheinlichkeit im full-Modus
#   sleep <sekunden>     Laufzeit, in beiden Modi
#   sleep-full <sek>     Laufzeit nur im full-Modus
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
      conflicts-with) [ "$mode" = full ] && present "$arg" && { echo "::error::$f: Konflikt mit $arg"; status=1; } ;;
      flaky) [ "$mode" = full ] && [ $((RANDOM % 100)) -lt "$arg" ] && { echo "::error::$f: flaky"; status=1; } ;;
      sleep) sleep "$arg" ;;
      sleep-full) [ "$mode" = full ] && sleep "$arg" ;;
      *) echo "::warning::$f: unbekanntes Flag $flag" ;;
    esac
  done < "$f"
done
[ "$status" = 0 ] && echo "Suite grün ($mode)"
exit "$status"
