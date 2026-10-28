# GitHub-Sandbox

- **Zweck:** E2E- und Differenztests gegen echtes GitHub (KONZEPT.md, Abschnitt 9). Schon vor
  dem ersten Schlange-Code nützlich: Die Szenarien zeichnen auf, wie Mergify sich verhält.
  Das ist das beobachtete Verhalten für den Clean-Room-Nachbau und der Sollwert für den
  Differenztest.
- **Code:** [sandbox/](../../sandbox/)

## Aufbau

Organisation `schlange-sandbox` (Free-Plan), Owner `karma-works`. Die Repos sind öffentlich,
weil der Free-Plan Rulesets nur für öffentliche Repos anbietet. Actions-Minuten sind dort
kostenlos und Mergify ist es für Open Source auch. Der Inhalt ist synthetisch.

| Repo | Queue | Schlange | Zweck |
|---|---|---|---|
| `queue-lab` | Mergify | Schatten (nur lesen) | Differenztest: dieselben PRs, Entscheidungen vergleichen |
| `schlange-e2e` | Schlange | aktiv | E2E: Queue-PRs, Rulesets, Merges |

Beide Repos bekommen dasselbe Fixture ([sandbox/fixture/](../../sandbox/fixture/)):

- `ci.yml` mit Pflichtergebnis `ci-ok`, `light` auf PRs und `full` auf `mergify/merge-queue/*`.
- `pr-gate.yml` mit dem Check `Risk class and data protection` (prüft den Abschnitt Datenschutz).
  Der Queue-Bot-Login steht in der Repo-Variablen `QUEUE_BOT_LOGIN` (die Zeile, die sich beim
  Umstieg ändert).
- `ai-review.yml` mit dem Check-Run `KI-Review` über `pull_request_target`, ohne Modell. Rot,
  wenn die PR-Beschreibung „KI-Review: rot“ enthält.
- Die `.mergify.yml` von Arztagent, nur die Logins sind ausgetauscht.
- `package.json` und `dependabot.yml` mit absichtlich alten Abhängigkeiten. Dependabot öffnet
  echte Patch-, Minor- und Major-PRs, dazu ein Update von `actions/checkout`, das einen
  Verfassungspfad ändert.
- Ruleset „main geschützt“: PR nötig, Checks `ci-ok` und `Risk class and data protection`,
  Ausnahme für die Queue-App (Mergify bzw. Schlange) und für Admins.

Die Suite wird über Flag-Dateien gesteuert ([ci/run.sh](../../sandbox/fixture/ci/run.sh)):
`fail`, `fail-light`, `fail-full`, `conflicts-with <pr>`, `flaky <prozent>`, `sleep`,
`sleep-full`. Es zählen nur Flags, die der PR bzw. das Bündel selbst ändert. Flags, die schon
auf `main` liegen, wirken nicht mehr.

## Identitäten

Es gibt keinen Maschinen-User. Alle Bots sind GitHub Apps: Sie haben eigene Logins und eng
gefasste Rechte, und Passwort und 2FA entfallen.

| Identität | Rolle |
|---|---|
| App „Schlange Sandbox Queue“ | Schlange als Queue in `schlange-e2e`, Schatten in `queue-lab` |
| App „Schlange Sandbox Driver“ | Test-Akteur: öffnet PRs, pusht, kommentiert. Gleichzeitig der *nicht* erlaubte Sender für `commands_restrictions` |
| Mergify | Referenz in `queue-lab` |
| `karma-works` (gh-Login) | der erlaubte Sender für `@mergifyio queue`; bewegt `main` per Admin-Ausnahme |

App-Schlüssel und Webhook-Secrets liegen nur lokal unter `~/.config/schlange-sandbox/apps/`
(Verzeichnis 0700, Dateien 0600).

## Einrichtung

### Was nur ein Mensch tun kann (ca. 15 Minuten)

1. **Organisation anlegen:** <https://github.com/account/organizations/new?plan=free>,
   Name `schlange-sandbox`. Per API ist das nicht möglich.
2. **Zwei Apps registrieren:** `bash sandbox/setup.sh apps` öffnet für jede App den Browser.
   Dort genügt ein Klick auf „Create GitHub App“. Ist der Name schon vergeben, kannst du ihn
   dort ändern.
3. **Beide Apps installieren:** Die Links gibt das Skript aus. Dort „Only select repositories“
   wählen, dann `queue-lab` und `schlange-e2e`.
4. **Mergify installieren**, nur für `queue-lab`:
   <https://github.com/apps/mergify/installations/new>. Danach im Mergify-Dashboard für
   `queue-lab` die Produkte **Merge Queue** und **Workflow Automation** einschalten. Ohne das
   zweite wertet Mergify die `pull_request_rules` nicht aus, und es wird nichts automatisch
   eingereiht. Vor dem Einschalten geöffnete PRs brauchen ein `@mergifyio refresh`.
5. **Mergify-API-Key** im Mergify-Dashboard anlegen und speichern unter
   `~/.config/schlange-sandbox/mergify-api-key` (chmod 600). Er wird für den Differenzvergleich
   gebraucht.
6. **Fine-grained Token** für den „Menschen“ im Driver:
   <https://github.com/settings/personal-access-tokens/new>, Resource owner `schlange-sandbox`,
   alle Repos, Contents/Pull requests/Issues: write, Administration: read. Speichern unter
   `~/.config/schlange-sandbox/human-token` (chmod 600). Er sieht nur die Sandbox.

### Was das Skript macht

```bash
bash sandbox/setup.sh check      # Stand, ändert nichts
bash sandbox/setup.sh all        # smee-Kanal, Apps, Repos (idempotent)
```

Mit `--dry-run` zeigt das Skript nur an, was es tun würde. Die Reihenfolge, wenn du alles einzeln
machen willst: Organisation anlegen → `setup.sh smee` → `setup.sh apps` (Klicks, dann
installieren) → `setup.sh repos` → Mergify installieren → `setup.sh check`.

`setup.sh repos` legt die Repos an und schreibt das Fixture. Außerdem stellt es die
Merge-Methoden, das Label `halt`, `QUEUE_BOT_LOGIN` und das Ruleset ein. `setup.sh fixture`
schreibt das Fixture neu auf `main`; das setzt den Inhalt zurück.

Webhooks: `setup.sh smee` legt einen Kanal auf smee.io an. Die Queue-App schickt ihre
Webhooks dorthin, lokal leitet `npx smee-client -u <kanal> -t http://127.0.0.1:8080/webhook`
sie weiter. Für den Dauerbetrieb zeigt die Webhook-URL der App auf die Schlange-Instanz auf dem
VPS.

## Szenarien

```bash
python3 -I sandbox/driver/sandbox.py run sandbox/scenarios/s03-rotes-buendel.json --repo queue-lab
python3 -I sandbox/driver/sandbox.py reset --repo queue-lab   # offene Szenario-PRs schließen
```

| ID | Inhalt | Katalog |
|---|---|---|
| S01 | ein PR, alles grün | MQ-18, MQ-20, MQ-42 |
| S02 | drei PRs als ein Bündel | MQ-26, MQ-20 |
| S03 | rotes Bündel, Bisektion | MQ-21, MQ-23, MQ-45 |
| S04 | einzeln grün, zusammen rot | MQ-01, MQ-21 |
| S05 | Label `halt` während der Queue | MQ-11, MQ-12, MQ-45 |
| S06 | Push während der Queue | MQ-07, MQ-14 |
| S07 | Entwurf, ready, wieder Entwurf | MQ-09 |
| S08 | Verfassungspfad, `@mergifyio queue` von erlaubtem und nicht erlaubtem Sender | MQ-50, MQ-52 |
| S09 | rote Schnellprüfung, fehlender Datenschutz, rotes KI-Review | MQ-16, MQ-37 |
| S10 | `main` bewegt sich, Textkonflikt mit `main` | MQ-06, MQ-27, MQ-47 |
| S11 | `@mergifyio dequeue` und wieder `queue` | MQ-45, MQ-50 |

Jeder Lauf schreibt `.sandbox-runs/<lauf>/` (nicht eingecheckt):

- `scenario.json`
- `run.json` (PR-Nummern)
- `trace.json`: Zeitachse je PR, Bot-Kommentare, Checks, Queue-PRs mit Commits, Commits auf `main` mit Tree-SHA
- `summary.json`: Kurzfassung

Die Aufzeichnungen enthalten Texte von Mergify-Kommentaren und bleiben deshalb lokal (Clean-Room,
KONZEPT.md Abschnitt 2). Ins Repo kommen nur Erkenntnisse in eigenen Worten, als Gotchas in den
Feature-Dateien.

Ein Szenario dauert je nach Wartezeiten 5–40 Minuten. Mergify wartet bis zu 5 Minuten auf ein
volles Bündel (`batch_max_wait_time`).

## Automatisch (GitHub Actions)

Der Workflow [sandbox.yml](../../.github/workflows/sandbox.yml) im Schlange-Repo spielt die
Szenarien ab. Ein Agent startet ihn so:

```bash
gh workflow run sandbox.yml -f repo=queue-lab -f scenarios="s01-einzelner-pr s03-rotes-buendel"
gh run watch "$(gh run list -w sandbox.yml -L1 --json databaseId --jq '.[0].databaseId')"
```

- **Secrets:** `bash sandbox/ci/upload-secrets.sh` lädt App-Schlüssel, Webhook-Secret,
  Sandbox-Token und Mergify-Key als Actions-Secrets hoch (`--list` zeigt den Stand). Im Runner
  baut `sandbox/ci/write-config.sh` daraus `~/.config/schlange-sandbox` nach. Alle Secrets wirken
  nur in `schlange-sandbox`.
- **Sperre:** ein Lauf pro Sandbox-Repo (`concurrency`). Zusätzlich wartet der Driver, bis keine
  Szenario- oder Queue-PRs mehr offen sind, damit sich lokale Läufe und CI-Läufe nicht mischen.
- **Sollwerte:** `sandbox/baselines/<szenario>.json` im eigenen Format `outcome.json`, ohne
  Mergify-Texte. Ein geprüfter Mergify-Lauf wird zum Sollwert, indem man seine `outcome.json`
  dorthin kopiert. Danach vergleicht jeder Lauf: auf `queue-lab`, ob Mergify sich geändert hat,
  auf `schlange-e2e`, ob Schlange sich wie Mergify verhält (`sandbox.py compare`).
- **Zeitplan:** sonntags alle Szenarien auf `queue-lab` (ca. 3–4 Stunden; im öffentlichen
  Repo kostenlos).
- **Ergebnis:** Tabelle in der Job-Zusammenfassung. Als Artefakt (14 Tage) nur `outcome.json`,
  `run.json` und `scenario.json`; `trace.json` und `summary.json` enthalten Mergify-Texte und
  bleiben im Runner. Für die volle Aufzeichnung das Szenario lokal laufen lassen.
- **Sicherheit:** Der Workflow startet nur per `workflow_dispatch` (Schreibrecht nötig) und
  `schedule`, nie durch PRs aus Forks. Die Secrets wirken nur in `schlange-sandbox`.
