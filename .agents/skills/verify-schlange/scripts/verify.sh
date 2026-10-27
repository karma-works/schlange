#!/usr/bin/env bash
# Verification driver for Schlange (see ../SKILL.md).
#
#   verify.sh launch    build the release binary (and with --image the container image)
#   verify.sh doctor    run `schlange doctor` on the binary (and in the image)
#   verify.sh drive     run the mapped CLI paths and scenarios
#   verify.sh cleanup   remove only what this script started (containers labelled by it)
#   verify.sh all       launch, doctor, drive, cleanup; evidence in one directory
#
# Options: --image (also build and check the container), --feature <id> (names the evidence
# directory), --evidence <dir> (reuse a directory). Every step writes its output into the
# evidence directory and a PASS/FAIL line into proof.md. Exit 0 only when every claim passed.
set -uo pipefail

root="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$root"
step="${1:-all}"; shift || true
image=false; feature="all"; evidence=""
while [ $# -gt 0 ]; do
  case "$1" in
    --image) image=true ;;
    --feature) feature="$2"; shift ;;
    --evidence) evidence="$2"; shift ;;
    *) echo "unknown option $1" >&2; exit 64 ;;
  esac
  shift
done
evidence="${evidence:-$root/evidence/$(date -u +%Y%m%dT%H%M%SZ)-$feature}"
mkdir -p "$evidence"
proof="$evidence/proof.md"
[ -f "$proof" ] || printf '# Evidence %s\n\n- commit: %s\n- dirty files: %s\n\n' \
  "$(basename "$evidence")" "$(git rev-parse --short HEAD)" "$(git status --porcelain | wc -l)" >"$proof"
bin="$root/target/release/schlange"
label="schlange.verify=$(basename "$evidence")"
failed=0

claim() { # claim <PASS|FAIL> <text>
  echo "- **$1** $2" >>"$proof"
  echo "$1  $2"
  [ "$1" = PASS ] || failed=1
}

expect_exit() { # expect_exit <expected> <name> <command...>; output to <name>.txt
  local want="$1" name="$2"; shift 2
  "$@" >"$evidence/$name.txt" 2>&1
  local got=$?
  if [ "$got" = "$want" ]; then claim PASS "\`$name\`: exit $got as expected (\`$*\`)"
  else claim FAIL "\`$name\`: exit $got, expected $want (\`$*\`)"; fi
}

launch() {
  expect_exit 0 build cargo build --release --locked -p schlange
  if $image; then
    expect_exit 0 image-build docker build --label "$label" -t schlange:verify .
    expect_exit 0 image-size scripts/image-size.sh schlange:verify
  fi
}

doctor() {
  expect_exit 0 doctor "$bin" doctor
  "$bin" doctor --json >"$evidence/doctor.json" 2>&1
  if $image; then
    # No schema inside the container: doctor must still pass (WARN, not FAIL).
    # --security-opt no-new-privileges is left out: on the VPS (Docker 28, AppArmor) it makes
    # every image fail to exec, alpine included. Re-add it on a host where it works.
    expect_exit 0 image-doctor docker run --rm --label "$label" --read-only --cap-drop ALL \
      schlange:verify doctor
    docker image inspect schlange:verify --format \
      'user={{.Config.User}} entrypoint={{.Config.Entrypoint}} healthcheck={{.Config.Healthcheck.Test}}' \
      >"$evidence/image-config.txt" 2>&1
  fi
}

drive() {
  local g=crates/schlange-config/tests/golden
  expect_exit 0 validate-arztagent "$bin" config validate "$g/arztagent.yml"
  "$bin" config validate --json "$g/arztagent.yml" >"$evidence/validate-arztagent.json" 2>&1
  expect_exit 1 validate-bad-schema "$bin" config validate "$g/bad-schema.yml"
  expect_exit 1 validate-bad-conditions "$bin" config validate "$g/bad-conditions.yml"
  expect_exit 1 validate-bad-yaml "$bin" config validate "$g/bad-yaml.yml"
  expect_exit 2 validate-unsupported-strict "$bin" config validate --strict "$g/unsupported.yml"
  expect_exit 3 validate-no-schema "$bin" config validate --schema /nonexistent.json "$g/arztagent.yml"
  if grep -q 'bad-schema.yml:3:19: error\[schema\] /queue_rules/0/merge_method' "$evidence/validate-bad-schema.txt"; then
    claim PASS "invalid merge method is reported with path, line and column"
  else
    claim FAIL "invalid merge method is not reported with path, line and column"
  fi
  for s in scenarios/*.yaml; do
    local n; n="sim-$(basename "$s" .yaml)"
    expect_exit 0 "$n" "$bin" sim run "$s"
    "$bin" sim run --json "$s" >"$evidence/$n.json" 2>&1
  done
  expect_exit 0 support-matrix "$bin" support-matrix
}

cleanup() {
  local ids
  ids="$(docker ps -aq --filter "label=$label" 2>/dev/null || true)"
  [ -n "$ids" ] && docker rm -f $ids >/dev/null
  if $image; then docker image rm schlange:verify >/dev/null 2>&1 || true; fi
  claim PASS "cleanup: removed only containers labelled $label and the image schlange:verify"
}

case "$step" in
  launch) launch ;;
  doctor) doctor ;;
  drive) drive ;;
  cleanup) cleanup ;;
  all)
    launch; doctor; drive; cleanup
    if [ -s "$evidence/build.txt" ] && [ -s "$proof" ]; then
      claim PASS "evidence survives cleanup in $evidence"
    else
      claim FAIL "evidence missing after cleanup"
    fi ;;
  *) echo "unknown step $step" >&2; exit 64 ;;
esac
echo
echo "evidence: $evidence"
exit "$failed"
