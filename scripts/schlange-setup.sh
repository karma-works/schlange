#!/usr/bin/env bash
# Setup for Schlange on GitHub (the replacement for scripts/mergify-setup.sh).
#
#   SCHLANGE_APP_ID=123456 bash scripts/schlange-setup.sh setup
#   SCHLANGE_APP_ID=123456 bash scripts/schlange-setup.sh lock-main
#   SCHLANGE_APP_ID=123456 bash scripts/schlange-setup.sh status
#
# Runs with gh and jq. Idempotent; asks before every change (skip with --yes).
# Writes no secrets. SCHLANGE_REPO defaults to the current repository; the app id
# is the numeric GitHub App id (App settings) or its client id.
set -euo pipefail

REPO="${SCHLANGE_REPO:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
APP_ID="${SCHLANGE_APP_ID:-}"
BACKUP_ROOT="${SCHLANGE_BACKUP:-.cache/schlange-setup}"
LOCK_NAME="Merge only via Schlange"
# Repository role "admin" (GitHub's fixed id for rulesets).
ADMIN_ROLE=5
ASSUME_YES=false

die() { echo "Error: $*" >&2; exit 1; }
info() { echo "-> $*"; }

confirm() {
  [ "$ASSUME_YES" = true ] && return 0
  local answer
  read -r -p "$1 [y/N] " answer </dev/tty || return 1
  [[ "$answer" =~ ^[yY]$ ]]
}

preflight() {
  command -v gh >/dev/null || die "gh is missing"
  command -v jq >/dev/null || die "jq is missing"
  gh auth status >/dev/null 2>&1 || die "gh is not logged in (gh auth login --web --scopes repo,workflow)"
  local admin
  admin=$(gh api "repos/$REPO" --jq '.permissions.admin') || die "repository $REPO is not readable"
  [ "$admin" = true ] || die "the logged-in account is not an admin of $REPO"
  [ -n "$APP_ID" ] || die "set SCHLANGE_APP_ID to the GitHub App's numeric id"
  [[ "$APP_ID" =~ ^[0-9]+$ ]] || die "SCHLANGE_APP_ID must be numeric"
  info "logged in as $(gh api user --jq .login), admin of $REPO"
}

# A ruleset applies to main when its branch condition includes the default branch, main or all.
ON_MAIN='((.conditions.ref_name.include // []) | any(. == "~DEFAULT_BRANCH" or . == "refs/heads/main" or . == "~ALL"))'

main_rulesets() {
  local id
  for id in $(gh api "repos/$REPO/rulesets" --jq '.[].id'); do
    gh api "repos/$REPO/rulesets/$id" --jq "select($ON_MAIN) | .id"
  done
}

# The app needs no exception anywhere but main; remove one added by an earlier run.
remove_elsewhere() {
  local app_id="$1" id current
  for id in $(gh api "repos/$REPO/rulesets" --jq '.[].id'); do
    current=$(gh api "repos/$REPO/rulesets/$id")
    if jq -e --argjson app "$app_id" \
         "($ON_MAIN | not) and any(.bypass_actors[]?; .actor_type == \"Integration\" and .actor_id == \$app)" \
         <<<"$current" >/dev/null; then
      jq --argjson app "$app_id" \
         '.bypass_actors |= map(select(.actor_type != "Integration" or .actor_id != $app))' \
         <<<"$current" | ruleset_body \
        | gh api -X PUT "repos/$REPO/rulesets/$id" --input - >/dev/null
      info "removed the Schlange exception from \"$(jq -r .name <<<"$current")\" (not for main)"
    fi
  done
}

# Classic branch protection of main with push restrictions (users, teams, apps).
allow_classic() {
  local apps
  apps=$(gh api "repos/$REPO/branches/main/protection/restrictions" --jq '[.apps[].slug] | join(",")' 2>/dev/null) \
    || { info "no classic branch protection with push restrictions on main"; return; }
  local slug
  slug=$(gh api "apps/$APP_ID" --jq .slug 2>/dev/null) || die "app $APP_ID not found"
  if [[ ",$apps," == *",$slug,"* ]]; then
    info "the app ($slug) is already allowed in main's classic branch protection"
    return
  fi
  gh api -X POST "repos/$REPO/branches/main/protection/restrictions/apps" -f "apps[]=$slug" >/dev/null
  info "the app ($slug) is allowed in main's classic branch protection"
}

# Only the fields the API accepts when writing.
ruleset_body() {
  jq '{name, target, enforcement, bypass_actors, conditions, rules}'
}

backup() {
  local dir="$BACKUP_ROOT/$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$dir/rulesets"
  gh api "repos/$REPO/actions/workflows" --paginate \
    --jq '.workflows[] | {path, state}' | jq -s . >"$dir/workflows.json"
  local id
  for id in $(gh api "repos/$REPO/rulesets" --jq '.[].id'); do
    gh api "repos/$REPO/rulesets/$id" >"$dir/rulesets/$id.json"
  done
  gh api "repos/$REPO/branches/main/protection" >"$dir/main-protection.json" 2>/dev/null || true
  info "backup: $dir (workflow state, rulesets, main's branch protection; no secrets)"
  BACKUP_DIR="$dir"
}

allow_on_main() {
  local app_id="$1" ids id current name
  remove_elsewhere "$app_id"
  allow_classic
  ids=$(main_rulesets)
  if [ -z "$ids" ]; then
    info "no ruleset on main: the app needs no exception"
    return
  fi
  for id in $ids; do
    current=$(gh api "repos/$REPO/rulesets/$id")
    name=$(jq -r .name <<<"$current")
    if jq -e --argjson app "$app_id" \
         'any(.bypass_actors[]?; .actor_type == "Integration" and .actor_id == $app and .bypass_mode == "exempt")' \
         <<<"$current" >/dev/null; then
      info "the app ($app_id) is already exempt in \"${name}\""
      continue
    fi
    # An earlier entry with another mode is replaced: `always` can leave parts blocked.
    jq --argjson app "$app_id" \
       '.bypass_actors = ([.bypass_actors[]? | select(.actor_type != "Integration" or .actor_id != $app)]
          + [{actor_id: $app, actor_type: "Integration", bypass_mode: "exempt"}])' \
       <<<"$current" | ruleset_body \
      | gh api -X PUT "repos/$REPO/rulesets/$id" --input - >/dev/null
    info "the app ($app_id) is exempt in \"${name}\""
  done
}

installed_hint() {
  echo
  echo "Check that the Schlange app is installed only for $REPO:"
  echo "  https://github.com/settings/installations"
}

cmd_setup() {
  preflight
  confirm "Back up, exempt the app in main's rulesets, create the halt label?" || die "cancelled"
  backup
  allow_on_main "$APP_ID"
  gh label create halt -R "$REPO" --color B60205 \
    --description "Do not merge (Schlange does not queue it)" --force >/dev/null
  info "label halt present"
  installed_hint
  echo
  echo "What now applies on main: bash scripts/schlange-setup.sh status"
}

# The ruleset that leaves merges on main to Schlange (exempt) and admins through a PR. Rule
# `update` blocks every other push and merge to the default branch.
LOCK_WANTED='[{actor_id: $app, actor_type: "Integration", bypass_mode: "exempt"},
  {actor_id: $admin, actor_type: "RepositoryRole", bypass_mode: "pull_request"}]'
LOCK_NEW="{name: \$name, target: \"branch\", enforcement: \"active\",
  conditions: {ref_name: {include: [\"~DEFAULT_BRANCH\"], exclude: []}},
  rules: [{type: \"update\"}], bypass_actors: $LOCK_WANTED}"
# An existing ruleset keeps its other rules and exceptions; only what is missing is added.
LOCK_MERGE=".enforcement = \"active\"
  | .conditions.ref_name.include = ((.conditions.ref_name.include // []) + [\"~DEFAULT_BRANCH\"] | unique)
  | .rules = ((.rules // []) + [{type: \"update\"}] | unique_by(.type))
  | .bypass_actors = ((.bypass_actors // []) | map(select(.actor_type != \"Integration\" or .actor_id != \$app))
      + [{actor_id: \$app, actor_type: \"Integration\", bypass_mode: \"exempt\"}]
      + (if any(.[]; .actor_type == \"RepositoryRole\" and .actor_id == \$admin) then [] else
          [{actor_id: \$admin, actor_type: \"RepositoryRole\", bypass_mode: \"pull_request\"}] end))"

cmd_lock_main() {
  preflight
  local id current name
  id=$(gh api "repos/$REPO/rulesets" --jq ".[] | select(.name == \"$LOCK_NAME\") | .id" | head -1)
  if [ -n "$id" ]; then
    current=$(gh api "repos/$REPO/rulesets/$id")
    name=$(jq -r .name <<<"$current")
    confirm "Keep the other rules and add the Schlange exemption to \"${name}\"?" || die "cancelled"
    jq --argjson app "$APP_ID" --argjson admin "$ADMIN_ROLE" "$LOCK_MERGE" <<<"$current" \
      | ruleset_body | gh api -X PUT "repos/$REPO/rulesets/$id" --input - >/dev/null
  else
    confirm "Create the ruleset \"$LOCK_NAME\" (merges only via Schlange, admins via PR)?" || die "cancelled"
    jq -n --arg name "$LOCK_NAME" --argjson app "$APP_ID" --argjson admin "$ADMIN_ROLE" \
      "$LOCK_NEW" | gh api -X POST "repos/$REPO/rulesets" --input - >/dev/null
  fi
  info "ruleset \"$LOCK_NAME\" is in force"
}

cmd_status() {
  preflight
  echo "Repository: $REPO; app: $APP_ID"
  echo "Rulesets on main:"
  local id current
  for id in $(main_rulesets); do
    current=$(gh api "repos/$REPO/rulesets/$id")
    jq -r --argjson app "$APP_ID" '
      "  - \(.name) (\(.enforcement)); app exempt: " +
      (if any(.bypass_actors[]?; .actor_type == "Integration" and .actor_id == $app and .bypass_mode == "exempt")
       then "yes" else "NO" end)
    ' <<<"$current"
  done
  echo "Label halt:"
  gh label view halt -R "$REPO" --json name,description --jq '"  \(.name): \(.description)"' 2>/dev/null \
    || echo "  missing"
}

case "${1:-}" in
  setup) cmd_setup ;;
  lock-main) cmd_lock_main ;;
  status) cmd_status ;;
  *)
    echo "Usage: bash scripts/schlange-setup.sh {setup|lock-main|status} [--yes]" >&2
    exit 64
    ;;
esac
