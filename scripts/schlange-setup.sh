#!/usr/bin/env bash
# Setup for Schlange on GitHub (the replacement for scripts/mergify-setup.sh).
#
#   SCHLANGE_APP_ID=123456 bash scripts/schlange-setup.sh setup
#   SCHLANGE_APP_ID=123456 bash scripts/schlange-setup.sh lock-main
#   SCHLANGE_APP_ID=123456 bash scripts/schlange-setup.sh status
#
# Runs with gh and jq. Idempotent; asks before every change (skip with --yes).
# Writes no secrets. SCHLANGE_REPO defaults to the current repository; the app id
# is the numeric GitHub App id (App settings). Set SCHLANGE_APP_SLUG too when
# classic branch protection restricts app pushes.
set -euo pipefail

REPO="${SCHLANGE_REPO:-}"
APP_ID="${SCHLANGE_APP_ID:-}"
APP_SLUG="${SCHLANGE_APP_SLUG:-}"
BACKUP_ROOT="${SCHLANGE_BACKUP:-.cache/schlange-setup}"
LOCK_NAME="Merge only via Schlange"
LEGACY_LOCK_NAME="Merge nur über Mergify"
REVOKE_APP=""
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
  [ -n "$REPO" ] || REPO=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
  gh auth status >/dev/null 2>&1 || die "gh is not logged in (gh auth login --web --scopes repo,workflow)"
  local admin
  admin=$(gh api "repos/$REPO" --jq '.permissions.admin') || die "repository $REPO is not readable"
  [ "$admin" = true ] || die "the logged-in account is not an admin of $REPO"
  [ -n "$APP_ID" ] || die "set SCHLANGE_APP_ID to the GitHub App's numeric id"
  [[ "$APP_ID" =~ ^[0-9]+$ ]] || die "SCHLANGE_APP_ID must be numeric"
  DEFAULT_BRANCH=$(gh api "repos/$REPO" --jq .default_branch)
  [ -n "$DEFAULT_BRANCH" ] && [ "$DEFAULT_BRANCH" != null ] || die "cannot determine the default branch"
  [ "$DEFAULT_BRANCH" = main ] || die "this script manages main and requires main to be the default branch"
  info "logged in as $(gh api user --jq .login), admin of $REPO"
}

# GitHub ref-name conditions use fnmatch paths: an ordinary star does not cross a slash.
# Include-minus-exclude is essential: granting bypass on an excluded branch is unnecessary.
ON_MAIN='def ref_matches:
  if . == "~DEFAULT_BRANCH" then $default == "main"
  elif . == "~ALL" then true
  else (split("/") | map(if . == "**" then ".*" else
    split("") | map(if . == "*" then "[^/]*" elif . == "?" then "[^/]"
      elif test("[.\\\\+(){}^$|]") then "\\" + . else . end) | join("") end)
    | join("/")) as $pattern | "refs/heads/main" | test("^" + $pattern + "$") end;
  (.target == "branch") and
  ((.conditions.ref_name.include // []) | any(ref_matches)) and
  (((.conditions.ref_name.exclude // []) | any(ref_matches)) | not)'

main_rulesets() {
  local id ids
  ids=$(gh api "repos/$REPO/rulesets" --jq '.[].id') || return 1
  for id in $ids; do
    gh api "repos/$REPO/rulesets/$id" | jq -r --arg default "$DEFAULT_BRANCH" "select($ON_MAIN) | .id" || return 1
  done
}

# The app needs no exception anywhere but main; remove one added by an earlier run.
remove_elsewhere() {
  local app_id="$1" id current ids
  ids=$(gh api "repos/$REPO/rulesets" --jq '.[].id') || return 1
  for id in $ids; do
    current=$(gh api "repos/$REPO/rulesets/$id") || return 1
    if jq -e --argjson app "$app_id" \
         --arg default "$DEFAULT_BRANCH" \
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
  verify_app_slug
  slug="$APP_SLUG"
  if [[ ",$apps," == *",$slug,"* ]]; then
    info "the app ($slug) is already allowed in main's classic branch protection"
    return
  fi
  gh api -X POST "repos/$REPO/branches/main/protection/restrictions/apps" -f "apps[]=$slug" >/dev/null
  info "the app ($slug) is allowed in main's classic branch protection"
}

verify_app_slug() {
  [ -n "$APP_SLUG" ] || die "set SCHLANGE_APP_SLUG when main has classic app push restrictions"
  [ "$(gh api "apps/$APP_SLUG" --jq .id)" = "$APP_ID" ] || die "SCHLANGE_APP_SLUG does not match SCHLANGE_APP_ID"
}

# Only the fields the API accepts when writing.
ruleset_body() {
  jq '{name, target, enforcement, bypass_actors, conditions, rules}'
}

backup() {
  mkdir -p "$BACKUP_ROOT"
  local dir
  dir=$(mktemp -d "$BACKUP_ROOT/$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
  mkdir -p "$dir/rulesets"
  gh api "repos/$REPO/actions/workflows" --paginate \
    --jq '.workflows[] | {path, state}' | jq -s . >"$dir/workflows.json"
  local id ids
  ids=$(gh api "repos/$REPO/rulesets" --jq '.[].id') || return 1
  for id in $ids; do
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
  local id current name locks
  locks=$(update_locks)
  if jq -e --arg name "$LOCK_NAME" 'any(.[]; .name != $name)' <<<"$locks" >/dev/null; then
    die "another update lock already covers main; use cutover-from-mergify to migrate it instead"
  fi
  [ "$(jq length <<<"$locks")" -le 1 ] || die "multiple update locks cover main; resolve them before locking"
  id=$(jq -r '.[0].id // empty' <<<"$locks")
  if [ -n "$id" ]; then
    current=$(gh api "repos/$REPO/rulesets/$id")
    name=$(jq -r .name <<<"$current")
    confirm "Keep the other rules and add the Schlange exemption to \"${name}\"?" || die "cancelled"
    backup
    jq --argjson app "$APP_ID" --argjson admin "$ADMIN_ROLE" "$LOCK_MERGE" <<<"$current" \
      | ruleset_body | gh api -X PUT "repos/$REPO/rulesets/$id" --input - >/dev/null
  else
    confirm "Create the ruleset \"$LOCK_NAME\" (merges only via Schlange, admins via PR)?" || die "cancelled"
    backup
    jq -n --arg name "$LOCK_NAME" --argjson app "$APP_ID" --argjson admin "$ADMIN_ROLE" \
      "$LOCK_NEW" | gh api -X POST "repos/$REPO/rulesets" --input - >/dev/null
  fi
  info "ruleset \"$LOCK_NAME\" is in force"
}

# Inspect full rulesets: their summary entries do not contain conditions/rules.
update_locks() {
  local id ids
  ids=$(main_rulesets) || return 1
  for id in $ids; do
    gh api "repos/$REPO/rulesets/$id" --jq \
      'select(.target == "branch" and any(.rules[]?; .type == "update"))' || return 1
  done | jq -s .
}

cmd_cutover() {
  preflight
  [[ "$REVOKE_APP" =~ ^[0-9]+$ ]] || die "cutover requires --revoke-app <Mergify numeric app id>"
  [ "$REVOKE_APP" != "$APP_ID" ] || die "cannot revoke the Schlange app itself"
  local locks id current slug
  locks=$(update_locks)
  [ "$(jq length <<<"$locks")" -eq 1 ] || die "cutover needs exactly one main update lock; resolve missing or competing locks first"
  current=$(jq '.[0]' <<<"$locks")
  jq -e --arg old "$LEGACY_LOCK_NAME" --arg new "$LOCK_NAME" \
    '.name == $old or .name == $new' <<<"$current" >/dev/null \
    || die "unrecognised update lock; inspect it before migration"
  # A wrong old app id must not silently grant a second writer.
  jq -e --argjson old "$REVOKE_APP" --argjson app "$APP_ID" \
    'any(.bypass_actors[]?; .actor_type == "Integration" and (.actor_id == $old or .actor_id == $app))' \
    <<<"$current" >/dev/null || die "neither selected queue app occurs in the update lock"
  slug=mergify
  [ "$(gh api "apps/$slug" --jq .id)" = "$REVOKE_APP" ] || die "--revoke-app must identify the Mergify app"
  if gh api "repos/$REPO/branches/main/protection/restrictions" >/dev/null 2>&1; then
    verify_app_slug
  fi
  confirm "Both queues must be paused/drained. Back up and replace Mergify's main write access with Schlange?" || die "cancelled"
  backup
  id=$(jq -r .id <<<"$current")
  # Replace the exclusive lock in ONE request before granting other exemptions. No step
  # permits both queue apps through this lock. Unknown actors are removed from this lock.
  jq --arg name "$LOCK_NAME" --argjson app "$APP_ID" --argjson admin "$ADMIN_ROLE" \
    ".name = \$name | .enforcement = \"active\" | .bypass_actors = $LOCK_WANTED" \
    <<<"$current" | ruleset_body \
    | gh api -X PUT "repos/$REPO/rulesets/$id" --input - >/dev/null
  local rs ids
  ids=$(main_rulesets)
  for rs in $ids; do
    gh api "repos/$REPO/rulesets/$rs" \
      | jq --argjson old "$REVOKE_APP" \
        '.bypass_actors = [.bypass_actors[]? | select(.actor_type != "Integration" or .actor_id != $old)]' \
      | ruleset_body | gh api -X PUT "repos/$REPO/rulesets/$rs" --input - >/dev/null
  done
  # Classic restrictions are an additional gate; remove Mergify before adding Schlange.
  if gh api "repos/$REPO/branches/main/protection/restrictions" >/dev/null 2>&1; then
    gh api -X DELETE "repos/$REPO/branches/main/protection/restrictions/apps" -f "apps[]=$slug" >/dev/null
  fi
  allow_on_main "$APP_ID"
  info "cutover complete; keep Mergify paused, run status and doctor before starting Schlange"
}

cmd_status() {
  preflight
  echo "Repository: $REPO; app: $APP_ID"
  echo "Rulesets on main:"
  local id current ids
  ids=$(main_rulesets)
  for id in $ids; do
    current=$(gh api "repos/$REPO/rulesets/$id")
    jq -r --argjson app "$APP_ID" '
      "  - \(.name) (\(.enforcement)); app exempt: " +
      (if any(.bypass_actors[]?; .actor_type == "Integration" and .actor_id == $app and .bypass_mode == "exempt")
       then "yes" else "NO" end),
      (.bypass_actors[]? | select(.actor_type == "Integration") |
       "      Integration \(.actor_id): \(.bypass_mode // "always")")
    ' <<<"$current"
  done
  echo "Classic main push restrictions (all actors):"
  gh api "repos/$REPO/branches/main/protection/restrictions" \
    --jq '{apps: [.apps[]?.slug], users: [.users[]?.login], teams: [.teams[]?.slug]}' \
    || echo "  not present or unreadable (verify with an administrator)"
  echo "Label halt:"
  gh label view halt -R "$REPO" --json name,description --jq '"  \(.name): \(.description)"' 2>/dev/null \
    || echo "  missing"
}

usage() {
  echo "Usage: bash scripts/schlange-setup.sh [--yes] {setup|lock-main|status|cutover-from-mergify} [--yes] [--revoke-app ID]" >&2
  exit 64
}
COMMAND=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --yes) ASSUME_YES=true ;;
    --revoke-app) [ "$#" -ge 2 ] || usage; REVOKE_APP="$2"; shift ;;
    setup|lock-main|status|cutover-from-mergify) [ -z "$COMMAND" ] || usage; COMMAND="$1" ;;
    *) usage ;;
  esac
  shift
done
[ -z "$REVOKE_APP" ] || [ "$COMMAND" = cutover-from-mergify ] || usage
case "$COMMAND" in
  setup) cmd_setup ;;
  lock-main) cmd_lock_main ;;
  status) cmd_status ;;
  cutover-from-mergify) cmd_cutover ;;
  *) usage ;;
esac
