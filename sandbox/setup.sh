#!/usr/bin/env bash
# Setup of the GitHub sandbox for Schlange (docs/betrieb/sandbox.md).
#
#   bash sandbox/setup.sh check       # prerequisites and state, changes nothing
#   bash sandbox/setup.sh smee        # create a webhook channel on smee.io (no account)
#   bash sandbox/setup.sh apps        # register the queue and driver apps (one click each)
#   bash sandbox/setup.sh repos       # create repos, fixture, labels, variables, rulesets
#   bash sandbox/setup.sh fixture     # rewrite the fixture on main (after changes)
#   bash sandbox/setup.sh fixture-e2e # rewrite only schlange-e2e's fixture
#   bash sandbox/setup.sh all         # smee, apps, repos in turn
#
# Options: --dry-run (only show). Environment: SANDBOX_ORG (default schlange-sandbox).
# Idempotent. Needs gh (logged in as owner of the organisation), jq, git, python3.
# Writes no secrets into the repo; app keys live in ~/.config/schlange-sandbox/.
# SC2015: info/todo do not fail.
# shellcheck disable=SC2015
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ORG="${SANDBOX_ORG:-schlange-sandbox}"
if [ "$ORG" != schlange-sandbox ]; then
  echo "Error: only schlange-sandbox is allowed" >&2
  exit 1
fi
CONFIG="${SCHLANGE_SANDBOX_CONFIG:-$HOME/.config/schlange-sandbox}"
LAB=queue-lab        # Mergify active, Schlange in shadow mode (differential test)
E2E=schlange-e2e     # Schlange active, without Mergify (E2E)
# The name stays "main geschützt" because the existing rulesets in GitHub carry it
# and this script finds them by name. Renaming it would create a second ruleset.
RULESET="main geschützt"
ADMIN_ROLE=5         # fixed ruleset ID of the repo role admin
DRY=false

die() { echo "Error: $*" >&2; exit 1; }
info() { echo "→ $*"; }
todo() { echo "☐ $*"; }
run() { if [ "$DRY" = true ]; then echo "  (dry-run) $*"; else "$@"; fi; }

app_field() { # app_field <role> <field>
  local f="$CONFIG/apps/$1.json"
  [ -f "$f" ] && jq -r ".$2" "$f" || true
}

load_env() {
  # shellcheck disable=SC1091
  [ -f "$CONFIG/sandbox.env" ] && . "$CONFIG/sandbox.env"
  SMEE_URL="${SMEE_URL:-}"
}

save_env() {
  mkdir -p "$CONFIG" && chmod 700 "$CONFIG"
  printf 'SMEE_URL=%q\n' "$SMEE_URL" > "$CONFIG/sandbox.env"
}

preflight() {
  local t
  for t in gh jq git python3 curl; do command -v "$t" >/dev/null || die "$t is missing"; done
  gh auth status >/dev/null 2>&1 || die "gh is not logged in (gh auth login --web)"
  ME=$(gh api user --jq .login)
  gh api "orgs/$ORG" >/dev/null 2>&1 || die "Organisation $ORG does not exist. Please create it: https://github.com/account/organizations/new?plan=free (name: $ORG)"
  local role
  role=$(gh api "orgs/$ORG/memberships/$ME" --jq .role 2>/dev/null || echo none)
  [ "$role" = admin ] || die "$ME is not an owner of $ORG (role: $role)"
}

cmd_smee() {
  load_env
  if [ -n "$SMEE_URL" ]; then info "smee channel exists: $SMEE_URL"; return; fi
  [ "$DRY" = true ] && { echo "  (dry-run) create a new channel on smee.io"; return; }
  SMEE_URL=$(curl -fsS -o /dev/null -w '%{redirect_url}' https://smee.io/new)
  [[ "$SMEE_URL" == https://smee.io/* ]] || die "smee.io did not return a channel"
  save_env
  info "smee channel created: $SMEE_URL (forward with: npx smee-client -u $SMEE_URL -t http://127.0.0.1:8080/webhook)"
}

cmd_apps() {
  load_env
  local role
  for role in queue driver; do
    if [ -f "$CONFIG/apps/$role.json" ]; then
      info "App $role registered: $(app_field "$role" html_url) (Bot $(app_field "$role" bot_login))"
      continue
    fi
    todo "Register app $role: the browser opens, click \"Create GitHub App\" there"
    if [ "$role" = queue ]; then
      [ -n "$SMEE_URL" ] || info "Without a smee channel the webhook stays inactive (run setup.sh smee first)"
      run python3 -I "$HERE/apps/create_app.py" queue --org "$ORG" ${SMEE_URL:+--webhook-url "$SMEE_URL"}
    else
      run python3 -I "$HERE/apps/create_app.py" driver --org "$ORG"
    fi
  done
  for role in queue driver; do
    [ -f "$CONFIG/apps/$role.json" ] || continue
    todo "Install it if not done yet: $(app_field "$role" html_url)/installations/new → Only select repositories → ${LAB} and ${E2E}"
  done
}

# Render the fixture: replace the placeholders in .mergify.yml.
render_fixture() { # render_fixture <target-dir>
  local dest="$1" driver
  driver=$(app_field driver bot_login)
  [ -n "$driver" ] || die "Driver app is missing (setup.sh apps)"
  cp -R "$HERE/fixture/." "$dest/"
  sed -e "s/@DRIVER_LOGIN@/$driver/g" -e "s/@HUMAN_LOGIN@/$ME/g" \
    "$dest/.mergify.yml.tmpl" > "$dest/.mergify.yml"
  rm "$dest/.mergify.yml.tmpl"
}

push_fixture() { # push_fixture <repo> <message>
  local repo="$1" msg="$2" tmp
  tmp=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN
  if gh api "repos/$ORG/$repo/commits?per_page=1" >/dev/null 2>&1; then
    git -c credential.helper= -c 'credential.helper=!gh auth git-credential' \
      clone -q "https://github.com/$ORG/$repo.git" "$tmp/r"
    (cd "$tmp/r" && git rm -rq --ignore-unmatch . >/dev/null)
  else
    git init -q -b main "$tmp/r"
    git -C "$tmp/r" remote add origin "https://github.com/$ORG/$repo.git"
  fi
  render_fixture "$tmp/r"
  (
    cd "$tmp/r"
    git add -A
    if git diff --cached --quiet; then info "$repo: fixture unchanged"; exit 0; fi
    git -c user.name="$ME" -c user.email="$ME@users.noreply.github.com" commit -qm "$msg"
    run git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push -q origin main
    info "$repo: fixture written to main"
  )
}

ensure_repo() { # ensure_repo <repo> <description>
  if gh api "repos/$ORG/$1" >/dev/null 2>&1; then info "Repo $ORG/$1 exists"; return; fi
  run gh repo create "$ORG/$1" --public --description "$2" --disable-wiki >/dev/null
  info "Repo $ORG/$1 created (public: rulesets on the free plan, free Actions)"
}

configure_repo() { # configure_repo <repo> <queue-bot-login> <bypass-app-id>
  local repo="$1" bot="$2" app_id="$3" r="$ORG/$1"
  run gh api -X PATCH "repos/$r" -F allow_squash_merge=true -F allow_merge_commit=true \
    -F allow_rebase_merge=true -F delete_branch_on_merge=true -F has_projects=false >/dev/null
  run gh label create halt -R "$r" --color B60205 --description "Keeps the PR out of the queue" --force >/dev/null
  run gh variable set QUEUE_BOT_LOGIN -R "$r" --body "$bot" >/dev/null
  info "$repo: merge methods, label halt, QUEUE_BOT_LOGIN=$bot"

  ruleset_apply "$r" "$app_id" true && {
    info "$repo: ruleset \"${RULESET}\" (checks ci-ok and Risk class…, bypass for app $app_id and admins)"
    return
  }
  # GitHub accepts an app as a bypass actor only once it is installed in the organisation.
  ruleset_apply "$r" "$app_id" false || die "$repo: could not create the ruleset"
  info "$repo: ruleset \"${RULESET}\" without the app bypass for now (app $app_id is not installed yet)"
  todo "$repo: install app $app_id in $ORG, then run setup.sh repos again (adds the bypass)"
}

ruleset_apply() { # ruleset_apply <owner/repo> <app-id> <with-app-bypass>
  local r="$1" id body out
  body=$(jq -n --arg name "$RULESET" --argjson app "$2" --argjson with_app "$3" --argjson admin "$ADMIN_ROLE" '{
    name: $name, target: "branch", enforcement: "active",
    conditions: {ref_name: {include: ["~DEFAULT_BRANCH"], exclude: []}},
    bypass_actors: ([{actor_type: "RepositoryRole", actor_id: $admin, bypass_mode: "always"}]
      + (if $with_app then [{actor_type: "Integration", actor_id: $app, bypass_mode: "always"}] else [] end)),
    rules: [
      {type: "deletion"}, {type: "non_fast_forward"},
      {type: "pull_request", parameters: {required_approving_review_count: 0,
        dismiss_stale_reviews_on_push: false, require_code_owner_review: false,
        require_last_push_approval: false, required_review_thread_resolution: false}},
      {type: "required_status_checks", parameters: {strict_required_status_checks_policy: false,
        required_status_checks: [{context: "ci-ok"}, {context: "Risk class and data protection"}]}}
    ]}')
  id=$(gh api "repos/$r/rulesets" --jq ".[] | select(.name == \"$RULESET\") | .id" 2>/dev/null || true)
  [ "$DRY" = true ] && { echo "  (dry-run) ruleset $RULESET in $r (app bypass: $3)"; return 0; }
  if [ -n "$id" ]; then
    out=$(gh api -X PUT "repos/$r/rulesets/$id" --input - <<<"$body" 2>&1) && return 0
  else
    out=$(gh api -X POST "repos/$r/rulesets" --input - <<<"$body" 2>&1) && return 0
  fi
  # A missing installation is expected; report anything else.
  [[ "$out" == *"must be part of the ruleset source or owner organization"* ]] || echo "$out" >&2
  return 1
}

cmd_repos() {
  local mergify_id queue_id queue_bot
  mergify_id=$(gh api apps/mergify --jq .id)
  queue_id=$(app_field queue id)
  queue_bot=$(app_field queue bot_login)
  [ -n "$queue_id" ] || die "Queue app is missing (setup.sh apps)"

  ensure_repo "$LAB" "Schlange sandbox: Mergify active, Schlange in shadow mode (differential test)"
  ensure_repo "$E2E" "Schlange sandbox: Schlange active (E2E)"
  if [ "$DRY" = true ]; then
    echo "  (dry-run) write the fixture, configure the repos"
  else
    push_fixture "$LAB" "Sandbox-Fixture"
    push_fixture "$E2E" "Sandbox-Fixture"
  fi
  configure_repo "$LAB" "mergify[bot]" "$mergify_id"
  configure_repo "$E2E" "$queue_bot" "$queue_id"
  todo "Install Mergify (only $ORG/$LAB): https://github.com/apps/mergify/installations/new"
  todo "Create a Mergify API key for $LAB (dashboard → API keys) and store it at: $CONFIG/mergify-api-key (chmod 600)"
}

cmd_fixture() {
  push_fixture "$LAB" "Update sandbox fixture"
  push_fixture "$E2E" "Update sandbox fixture"
}

cmd_check() {
  load_env
  info "Logged in as $ME, owner of $ORG"
  [ -n "$SMEE_URL" ] && info "smee channel: $SMEE_URL" || todo "smee channel is missing (setup.sh smee)"
  local role repo
  for role in queue driver; do
    if [ -f "$CONFIG/apps/$role.json" ]; then
      info "App $role: $(app_field "$role" bot_login)"
    else
      todo "App $role is missing (setup.sh apps)"
    fi
  done
  for repo in "$LAB" "$E2E"; do
    if gh api "repos/$ORG/$repo" >/dev/null 2>&1; then
      info "Repo $repo: ruleset $(gh api "repos/$ORG/$repo/rulesets" --jq "[.[] | select(.name == \"$RULESET\")] | length") ×, QUEUE_BOT_LOGIN=$(gh variable get QUEUE_BOT_LOGIN -R "$ORG/$repo" 2>/dev/null || echo –)"
    else
      todo "Repo $repo is missing (setup.sh repos)"
    fi
  done
  python3 -I "$HERE/driver/sandbox.py" doctor --org "$ORG" || true
  [ -f "$CONFIG/mergify-api-key" ] && info "Mergify API key present" || todo "Mergify API key is missing ($CONFIG/mergify-api-key)"
}

main() {
  local cmd="${1:-}"
  shift || true
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run) DRY=true ;;
      *) die "unknown option $1" ;;
    esac
    shift
  done
  case "$cmd" in
    check) preflight; cmd_check ;;
    smee) cmd_smee ;;
    apps) preflight; cmd_apps ;;
    repos) preflight; cmd_repos ;;
    fixture) preflight; cmd_fixture ;;
    fixture-e2e) preflight; push_fixture "$E2E" "Update sandbox fixture" ;;
    all) preflight; cmd_smee; cmd_apps; cmd_repos ;;
    *) sed -n '2,13p' "$0"; exit 2 ;;
  esac
}

main "$@"
