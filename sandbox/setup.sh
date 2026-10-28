#!/usr/bin/env bash
# Einrichtung der GitHub-Sandbox für Schlange (docs/betrieb/sandbox.md).
#
#   bash sandbox/setup.sh check       # Voraussetzungen und Stand, ändert nichts
#   bash sandbox/setup.sh smee        # Webhook-Kanal auf smee.io anlegen (ohne Konto)
#   bash sandbox/setup.sh apps        # Queue- und Driver-App registrieren (je ein Klick)
#   bash sandbox/setup.sh repos       # Repos anlegen, Fixture, Labels, Variablen, Rulesets
#   bash sandbox/setup.sh fixture     # Fixture-Stand neu auf main schreiben (nach Änderungen)
#   bash sandbox/setup.sh all         # smee, apps, repos nacheinander
#
# Optionen: --dry-run (nur anzeigen). Umgebung: SANDBOX_ORG (Default schlange-sandbox).
# Idempotent. Läuft mit gh (angemeldet als Owner der Organisation), jq, git, python3.
# Schreibt keine Secrets ins Repo; App-Schlüssel liegen unter ~/.config/schlange-sandbox/.
# SC1111: deutsche Anführungszeichen in Meldungen sind gewollt. SC2015: info/todo scheitern nicht.
# shellcheck disable=SC1111,SC2015
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ORG="${SANDBOX_ORG:-schlange-sandbox}"
CONFIG="${SCHLANGE_SANDBOX_CONFIG:-$HOME/.config/schlange-sandbox}"
LAB=queue-lab        # Mergify aktiv, Schlange im Schatten (Differenztest)
E2E=schlange-e2e     # Schlange aktiv, ohne Mergify (E2E)
RULESET="main geschützt"
ADMIN_ROLE=5         # feste Ruleset-ID der Repo-Rolle admin
DRY=false

die() { echo "Fehler: $*" >&2; exit 1; }
info() { echo "→ $*"; }
todo() { echo "☐ $*"; }
run() { if [ "$DRY" = true ]; then echo "  (dry-run) $*"; else "$@"; fi; }

app_field() { # app_field <rolle> <feld>
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
  for t in gh jq git python3 curl; do command -v "$t" >/dev/null || die "$t fehlt"; done
  gh auth status >/dev/null 2>&1 || die "gh ist nicht angemeldet (gh auth login --web)"
  ME=$(gh api user --jq .login)
  gh api "orgs/$ORG" >/dev/null 2>&1 || die "Organisation $ORG gibt es nicht. Bitte anlegen: https://github.com/account/organizations/new?plan=free (Name: $ORG)"
  local role
  role=$(gh api "orgs/$ORG/memberships/$ME" --jq .role 2>/dev/null || echo none)
  [ "$role" = admin ] || die "$ME ist nicht Owner von $ORG (Rolle: $role)"
}

cmd_smee() {
  load_env
  if [ -n "$SMEE_URL" ]; then info "smee-Kanal vorhanden: $SMEE_URL"; return; fi
  [ "$DRY" = true ] && { echo "  (dry-run) neuen Kanal auf smee.io anlegen"; return; }
  SMEE_URL=$(curl -fsS -o /dev/null -w '%{redirect_url}' https://smee.io/new)
  [[ "$SMEE_URL" == https://smee.io/* ]] || die "smee.io lieferte keinen Kanal"
  save_env
  info "smee-Kanal angelegt: $SMEE_URL (Weiterleitung: npx smee-client -u $SMEE_URL -t http://127.0.0.1:8080/webhook)"
}

cmd_apps() {
  load_env
  local role
  for role in queue driver; do
    if [ -f "$CONFIG/apps/$role.json" ]; then
      info "App $role registriert: $(app_field "$role" html_url) (Bot $(app_field "$role" bot_login))"
      continue
    fi
    todo "App $role registrieren: Browser öffnet sich, dort „Create GitHub App“ klicken"
    if [ "$role" = queue ]; then
      [ -n "$SMEE_URL" ] || info "Ohne smee-Kanal bleibt der Webhook inaktiv (vorher: setup.sh smee)"
      run python3 -I "$HERE/apps/create_app.py" queue --org "$ORG" ${SMEE_URL:+--webhook-url "$SMEE_URL"}
    else
      run python3 -I "$HERE/apps/create_app.py" driver --org "$ORG"
    fi
  done
  for role in queue driver; do
    [ -f "$CONFIG/apps/$role.json" ] || continue
    todo "Installieren, falls noch nicht: $(app_field "$role" html_url)/installations/new → Only select repositories → $LAB und $E2E"
  done
}

# Fixture rendern: Platzhalter in .mergify.yml ersetzen.
render_fixture() { # render_fixture <zielverzeichnis>
  local dest="$1" driver
  driver=$(app_field driver bot_login)
  [ -n "$driver" ] || die "Driver-App fehlt (setup.sh apps)"
  cp -R "$HERE/fixture/." "$dest/"
  sed -e "s/@DRIVER_LOGIN@/$driver/g" -e "s/@HUMAN_LOGIN@/$ME/g" \
    "$dest/.mergify.yml.tmpl" > "$dest/.mergify.yml"
  rm "$dest/.mergify.yml.tmpl"
}

push_fixture() { # push_fixture <repo> <nachricht>
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
    if git diff --cached --quiet; then info "$repo: Fixture unverändert"; exit 0; fi
    git -c user.name="$ME" -c user.email="$ME@users.noreply.github.com" commit -qm "$msg"
    run git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push -q origin main
    info "$repo: Fixture auf main geschrieben"
  )
}

ensure_repo() { # ensure_repo <repo> <beschreibung>
  if gh api "repos/$ORG/$1" >/dev/null 2>&1; then info "Repo $ORG/$1 vorhanden"; return; fi
  run gh repo create "$ORG/$1" --public --description "$2" --disable-wiki >/dev/null
  info "Repo $ORG/$1 angelegt (öffentlich: Rulesets im Free-Plan, Actions gratis)"
}

configure_repo() { # configure_repo <repo> <queue-bot-login> <bypass-app-id>
  local repo="$1" bot="$2" app_id="$3" r="$ORG/$1"
  run gh api -X PATCH "repos/$r" -F allow_squash_merge=true -F allow_merge_commit=true \
    -F allow_rebase_merge=true -F delete_branch_on_merge=true -F has_projects=false >/dev/null
  run gh label create halt -R "$r" --color B60205 --description "Hält den PR aus der Queue" --force >/dev/null
  run gh variable set QUEUE_BOT_LOGIN -R "$r" --body "$bot" >/dev/null
  info "$repo: Merge-Methoden, Label halt, QUEUE_BOT_LOGIN=$bot"

  ruleset_apply "$r" "$app_id" true && {
    info "$repo: Ruleset „${RULESET}“ (Checks ci-ok und Risk class…, Ausnahme App $app_id und Admins)"
    return
  }
  # GitHub nimmt eine App erst als Ausnahme an, wenn sie in der Organisation installiert ist.
  ruleset_apply "$r" "$app_id" false || die "$repo: Ruleset ließ sich nicht anlegen"
  info "$repo: Ruleset „${RULESET}“ vorerst ohne App-Ausnahme (App $app_id ist noch nicht installiert)"
  todo "$repo: App $app_id in $ORG installieren, dann setup.sh repos erneut ausführen (trägt die Ausnahme nach)"
}

ruleset_apply() { # ruleset_apply <owner/repo> <app-id> <mit-app-ausnahme>
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
  [ "$DRY" = true ] && { echo "  (dry-run) Ruleset $RULESET in $r (App-Ausnahme: $3)"; return 0; }
  if [ -n "$id" ]; then
    out=$(gh api -X PUT "repos/$r/rulesets/$id" --input - <<<"$body" 2>&1) && return 0
  else
    out=$(gh api -X POST "repos/$r/rulesets" --input - <<<"$body" 2>&1) && return 0
  fi
  # Fehlende Installation ist erwartbar; alles andere melden.
  [[ "$out" == *"must be part of the ruleset source or owner organization"* ]] || echo "$out" >&2
  return 1
}

cmd_repos() {
  local mergify_id queue_id queue_bot
  mergify_id=$(gh api apps/mergify --jq .id)
  queue_id=$(app_field queue id)
  queue_bot=$(app_field queue bot_login)
  [ -n "$queue_id" ] || die "Queue-App fehlt (setup.sh apps)"

  ensure_repo "$LAB" "Schlange-Sandbox: Mergify aktiv, Schlange im Schatten (Differenztest)"
  ensure_repo "$E2E" "Schlange-Sandbox: Schlange aktiv (E2E)"
  if [ "$DRY" = true ]; then
    echo "  (dry-run) Fixture schreiben, Repos konfigurieren"
  else
    push_fixture "$LAB" "Sandbox-Fixture"
    push_fixture "$E2E" "Sandbox-Fixture"
  fi
  configure_repo "$LAB" "mergify[bot]" "$mergify_id"
  configure_repo "$E2E" "$queue_bot" "$queue_id"
  todo "Mergify installieren (nur $ORG/$LAB): https://github.com/apps/mergify/installations/new"
  todo "Mergify-API-Key für $LAB anlegen (Dashboard → API keys) und ablegen: $CONFIG/mergify-api-key (chmod 600)"
}

cmd_fixture() {
  push_fixture "$LAB" "Sandbox-Fixture aktualisieren"
  push_fixture "$E2E" "Sandbox-Fixture aktualisieren"
}

cmd_check() {
  load_env
  info "Angemeldet als $ME, Owner von $ORG"
  [ -n "$SMEE_URL" ] && info "smee-Kanal: $SMEE_URL" || todo "smee-Kanal fehlt (setup.sh smee)"
  local role repo
  for role in queue driver; do
    if [ -f "$CONFIG/apps/$role.json" ]; then
      info "App $role: $(app_field "$role" bot_login)"
    else
      todo "App $role fehlt (setup.sh apps)"
    fi
  done
  for repo in "$LAB" "$E2E"; do
    if gh api "repos/$ORG/$repo" >/dev/null 2>&1; then
      info "Repo $repo: Ruleset $(gh api "repos/$ORG/$repo/rulesets" --jq "[.[] | select(.name == \"$RULESET\")] | length") ×, QUEUE_BOT_LOGIN=$(gh variable get QUEUE_BOT_LOGIN -R "$ORG/$repo" 2>/dev/null || echo –)"
    else
      todo "Repo $repo fehlt (setup.sh repos)"
    fi
  done
  python3 -I "$HERE/driver/sandbox.py" doctor --org "$ORG" || true
  [ -f "$CONFIG/mergify-api-key" ] && info "Mergify-API-Key vorhanden" || todo "Mergify-API-Key fehlt ($CONFIG/mergify-api-key)"
}

main() {
  local cmd="${1:-}"
  shift || true
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run) DRY=true ;;
      *) die "unbekannte Option $1" ;;
    esac
    shift
  done
  case "$cmd" in
    check) preflight; cmd_check ;;
    smee) cmd_smee ;;
    apps) preflight; cmd_apps ;;
    repos) preflight; cmd_repos ;;
    fixture) preflight; cmd_fixture ;;
    all) preflight; cmd_smee; cmd_apps; cmd_repos ;;
    *) sed -n '2,13p' "$0"; exit 2 ;;
  esac
}

main "$@"
