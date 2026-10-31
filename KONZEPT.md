# Schlange – Konzept

- **Stand:** 27.10.2026, Entwurf zur Entscheidung
- **Ziel:** Open-Source-Merge-Queue, die Mergify für Arztagent ersetzt, ohne dass sich die
  `.mergify.yml` ändert. Selbst betrieben in einem sehr kleinen, gehärteten Container,
  vollständig per CLI und MCP bedienbar.
- **Recherche:** [docs/research/](docs/research/) (Mergify-Features, Merge-Queue-Probleme,
  Open-Source-Landschaft, Mergify-Schwächen). Lokale Kopien der Mergify-Doku, des
  Konfigurationsschemas und der OpenAPI-Spezifikation liegen in `.research/mergify/`
  (nicht eingecheckt, Urheberrecht Mergify).

## 1. Kurzurteil

Machbar, wenn der Umfang hart begrenzt bleibt. Arztagent nutzt nur einen kleinen Teil von
Mergify (Abschnitt 3). Diesen Teil exakt nachzubauen, ist ein überschaubares Projekt. Den
ganzen Mergify-Umfang nachzubauen, ist es nicht: Das Schema hat 236 Konfigurationspfade,
76 PR-Attribute, 18 Aktionen und 79 API-Operationen, dazu CI Insights, Test Engine, Stacks
und ein Dashboard.

Das größte Risiko ist nicht die Technik, sondern die Falle, aus der Arztagent gerade kommt:
Laut PLAN-MERGIFY.md wurden ca. 8.600 Zeilen eigener Merge-Controller durch Mergify ersetzt,
weil jeder Grenzfall neuen Code erzeugte. Schlange vermeidet das nur mit drei Regeln:

1. **Erst simulieren, dann bauen.** Jeder Grenzfall ist zuerst ein Szenario im simulierten
   Forge, dann Code.
2. **Kein Feature ohne Nachweis.** Der Implementation Agent belegt jedes Feature mit dem
   Verification Skill, der Kritiker bewertet es gegen die Mergify-Doku.
3. **Schattenbetrieb vor Umstieg.** Schlange läuft auf Arztagent zuerst nur beobachtend neben
   Mergify und muss dieselben Entscheidungen treffen, bevor es selbst merged.

## 2. Kritik: warum es scheitern kann, und die Gegenmaßnahme

| Problem | Warum | Gegenmaßnahme |
|---|---|---|
| „100 % Drop-in“ ist auf GitHub nicht ganz erreichbar | Der Bot heißt nicht `mergify[bot]`. `pr-gate.yml` prüft genau diesen Login. Mergify-eigene Check-Namen und die Branch-Ausnahme in den Rulesets gelten für die Mergify-App. | Konfiguration bleibt 1:1. Queue-Branch-Präfix bleibt `mergify/merge-queue/`, Befehle `@mergifyio …` werden verstanden. Die Workflows brauchen eine Zeile mehr (Bot-Login), ein Verfassungs-PR durch einen Menschen. Ruleset-Ausnahme für die Schlange-App per Skript wie `scripts/schlange-setup.sh setup`. |
| Python-Regex-Semantik | Die `files`-Bedingung nutzt negativen Lookahead (`^(?!…)`). Rusts `regex` und Gos `regexp` können das nicht. | Rust: `fancy-regex` (im Prototyp geprüft: kompiliert und filtert richtig). Go: `regexp2`. Differenztest gegen Pythons `re` mit erzeugten Pfaden. |
| Merge Queue ist kritische Infrastruktur | Am 23.04.2026 hat GitHubs Merge Queue bei Squash-Bündeln 2.092 PRs falsch gelandet (getesteter ≠ gelandeter Stand). Wer selbst betreibt, trägt Ausfälle selbst. | Invariante I0 (Abschnitt 6) nach jedem Merge prüfen, bei Abweichung Queue anhalten und Alarm. Mergify bleibt bis zur Abnahme als Rückfallweg installiert. |
| Verborgene Defaults | Arztagent setzt vieles nicht und lebt von Mergify-Defaults: `max_parallel_checks` 5, `checks_timeout`, Bisektion ohne Versuchslimit, Labels, Status-Kommentare, Commit-Nachricht beim Squash. | Jeder Default ist ein eigener Eintrag in der Feature-Liste, mit Quelle im Schema oder in der Doku. |
| Lizenz | Die Mergify-CLI ist seit 08.10.2026 proprietär und verbietet ausdrücklich den Einsatz für Konkurrenzprodukte. Der Engine-Code ist seit 2022 nicht mehr öffentlich. | Clean-Room: nur die öffentliche Doku, das Schema und beobachtetes Verhalten. Kein Code und kein Doku-Text von Mergify im Repo, Feature-Liste in eigenen Worten. Die Mergify-CLI wird nicht benutzt. |
| Lohnt es sich? | Mergify ist für bis zu 5 aktive Mitwirkende kostenlos. | Gründe dafür, belegt in [mergify-schwaechen.md](docs/research/mergify-schwaechen.md): Im April 2026 stand die Verarbeitung nur für Free-Tier-Konten ca. 23 h still (#5164). Im August und September 2026 gab es Hänger von 4–8 h und Verzögerungen von 30–118 min. Das Hosting ist unklar, einen AVV gibt es nur auf Anfrage, und die App braucht breite Rechte. Dazu ungefähr 20 Abkündigungen in zwei Jahren. Beim eigenen Betrieb bleiben Code und PR-Daten im Haus, und Erklärbarkeit und Agenten-Oberfläche sind besser. Dafür kostet Betrieb Aufmerksamkeit. |
| Mergify ist ein bewegliches Ziel | Drop-in heißt auch, Mergifys Änderungen nachzuziehen. Jinja2 endet am 31.10.2026, `allow_queue_branch_edit` am 31.12.2026. | Gepinntes Schema plus wöchentlicher Schema-Diff (Abschnitt 4). Kompatibilitätsziel ist das Verhalten, das Arztagent braucht, nicht jede Mergify-Version. |

## 3. Priorität: was Arztagent tatsächlich nutzt (Phase 1)

Abgeleitet aus `arzt-agent/.mergify.yml`, `ci.yml`, `pr-gate.yml`, `ai-review.yml`,
`AGENTS.md` und `PLAN-MERGIFY.md`.

**Konfiguration**
- `.mergify.yml` nur vom Default-Branch lesen; Suchreihenfolge der sechs Dateinamen.
- Top-Level: `queue_rules`, `pull_request_rules`, `commands_restrictions`. Alles andere wird
  vom Validator als „nicht unterstützt“ gemeldet.

**Queue (`queue_rules`)**
- `name`, `merge_method: squash`, `batch_size: 5`, `batch_max_wait_time: 5 minutes`.
- Two-Step-CI: `queue_conditions` (Einreihen) und `merge_conditions` (auf dem Queue-PR).
- Queue-PRs als Entwurfs-PR auf `mergify/merge-queue/<10 hex>`; sie erzeugen
  `pull_request`-Events, auf die `ci.yml` (`mode=full`), `ai-review.yml` und `pr-gate.yml`
  reagieren.
- Serielle, kumulative Speculation (PR 1, PR 1+2, …) mit `max_parallel_checks` 5 (Default).
- Bisektion eines roten Bündels, Rest wird gemerged; Abhängige werden neu aufgebaut.
- Automatisches Austragen, sobald `queue_conditions` nicht mehr gelten (Label `halt`,
  Entwurf, neuer Push, roter Check), mit Grund.
- Konflikt mit `main` oder mit einem PR davor; Base bewegt sich außerhalb der Queue.
- Default-`checks_timeout`, Squash-Commit-Nachricht (Default-Format), Labels und
  Status-Kommentare wie Mergify.

**Bedingungen**
- Attribute: `base`, `draft`, `label`, `#check-failure`, `check-success`, `author`, `files`,
  `dependabot-update-type`, `sender`.
- Operatoren `=`, `~=`, Negation `-`, Längenoperator `#`, Kombinator `or`.
- Check-Semantik exakt wie Mergify: neueste Check-Suite, `check-failure` umfasst auch
  cancelled, timed-out und action_required; „skipped“ ist kein Erfolg.

**Regeln und Befehle**
- `pull_request_rules` mit Aktion `queue` (ohne Namen: erste passende Queue).
- Befehle `@mergifyio queue`, `@mergifyio dequeue`, zusätzlich `refresh` für Betrieb.
- `commands_restrictions` mit `sender`; Default-Beschränkungen für die übrigen Befehle.
- Dependabot: `dependabot-update-type` aus den PR-Metadaten.

**GitHub-Anbindung**
- GitHub App mit Webhooks und periodischem Abgleich, Check-Runs, Ruleset-Ausnahme.

Alles Übrige aus dem Schema ist Phase 2, in der Reihenfolge des erwarteten Nutzens:
Prioritäten, Pause und Freeze, weitere Merge-/Update-Methoden, `checks_timeout` und
`max_checks_retries` als Option, `skip_intermediate_results`, Scopes, Merge Protections
(`Depends-On`, `Merge-After`), übrige Aktionen, `extends`/`defaults`, API. CI Insights,
Test Engine, Stacks und Dashboard zuletzt oder gar nicht.

## 4. Vollständige Feature-Liste aus dem Schema

Mergify veröffentlicht ein JSON Schema (Draft 2020-12) der Konfiguration
(`docs.mergify.com/mergify-configuration-schema.json`) und eine OpenAPI-Spezifikation. Daraus
entsteht die Liste maschinell, damit kein Feature fehlt:

1. Ein Skript `tools/inventory` extrahiert aus dem gepinnten Schema alle Optionspfade (236),
   alle PR-Attribute (76, `$defs.PullRequestAttributes`), alle Aktionen (18), alle
   Dequeue-Gründe, aus der OpenAPI alle Operationen (79), dazu die Befehle und die
   CLI-Kommandos aus der Doku. Ergebnis: `features/_inventory.json`.
2. Je Feature eine Datei in `features/` (Format des Verification Skills: Unterfeatures,
   Weg aus Nutzersicht, Ansteuerung, Gotchas), mit ID, Priorität (P1/P2/P3),
   Arztagent-Nutzung ja/nein, Status (fehlt/teilweise/konform) und den Inventar-Einträgen, die
   sie abdeckt.
3. Ein Test schlägt fehl, wenn ein Inventar-Eintrag keiner Feature-Datei zugeordnet ist.
   Ein Update des Schemas (wöchentlicher Job, Diff gegen die gepinnte Version) lässt neue
   Mergify-Features damit automatisch als offen erscheinen.
4. Eigene Schlange-Features bekommen das Präfix `S-` (Abschnitt 8).

## 5. Validator

`schlange config validate [--strict] [--json] <datei>`, Exit-Code 0 gültig, 1 ungültig,
2 gültig für Mergify, aber nicht unterstützt von Schlange.

| Stufe | Prüft | Werkzeug |
|---|---|---|
| 1 YAML | Syntax, Anker/Aliase (`shared`), doppelte Schlüssel, Zeilen- und Spaltennummern für jede Meldung | Rust: `serde_yaml_ng` (im Prototyp), für Positionen `saphyr`. Go: `goccy/go-yaml` |
| 2 Mergify-Schema | Typen, Enums, erlaubte Schlüssel, Grenzen (z. B. max. 50 Queues) gegen das gepinnte Schema | Rust: `jsonschema` (im Prototyp: Arztagent-Konfig gültig; `sqush` und `bach_size` werden mit Pfad gemeldet). Go: `santhosh-tekuri/jsonschema` oder `google/jsonschema-go` |
| 3 Bedingungen | Im Schema ist jede Bedingung nur ein String. Eigener Parser: Attribut existiert, Operator passt zum Typ, Wert parsebar (Dauer, Zeitplan, Zeitstempel), Regex kompiliert mit Python-Semantik, Globs | eigener Parser (`winnow` bzw. handgeschrieben), `fancy-regex` / `regexp2` |
| 4 Semantik | eindeutige Regelnamen, Verweise auf existierende Queues, unverträgliche Optionen (z. B. `fast-forward` mit `parallel`) | eigener Code |
| 5 Unterstützung | Jeder Pfad, jedes Attribut und jede Aktion gegen die Unterstützungsmatrix, die aus `features/` erzeugt wird. Meldung mit Feature-ID | generiert aus der Feature-Liste |

**Zur „Library von Google“:** Gemeint ist vermutlich CUE (bei Google entstanden) oder
`google/jsonschema-go`. CUE kann JSON Schema importieren und YAML prüfen, bringt aber eine
eigene Sprache mit, und die Quelle der Wahrheit ist ohnehin Mergifys JSON Schema. Es zusätzlich
nach CUE zu übersetzen, bringt keinen Gewinn. `google/jsonschema-go` (MIT, v0.4.3) ist eine gute
Wahl, falls wir Go nehmen; es ist die Basis des offiziellen MCP-Go-SDK. CEL (ebenfalls Google)
ist eine Ausdruckssprache und passt nicht zur Bedingungssyntax von Mergify. Empfehlung: JSON
Schema validieren, Bedingungen mit eigenem Parser, Unterstützung aus der Feature-Liste.

Prototyp der Stufen 1, 2 und des Regex-Teils von 3: [spikes/validator](spikes/validator/)
(`cargo run --release -- ../../.research/mergify/schema.json arztagent.mergify.yml`).

Offener Punkt für Tests: Mergify parst YAML mit Python. Unterschiede zwischen YAML 1.1 und 1.2
(`on`, `yes`, Oktalzahlen) sind ein eigener Differenztest.

## 6. Architektur

```
schlange (ein statisches Binary)
├── serve      Webhook-Empfang, Abgleich, Scheduler, HTTP-API, MCP über HTTP
├── mcp        MCP über stdio
└── CLI        config validate|simulate, queue status|show|pause, pr explain, sim run, doctor, events
```

- **Kern ohne Seiteneffekte:** `(Zustand, Ereignis) → (neuer Zustand, Befehle)`. Die Befehle
  (Branch anlegen, Queue-PR öffnen, mergen, kommentieren) führt ein Forge-Adapter aus. So ist
  der Kern deterministisch und in der Simulation vollständig testbar.
- **Forge-Adapter (eine Schnittstelle, vier Implementierungen):**
  1. `sim`: im Speicher, virtuelle Uhr, Fehlerinjektion (Webhooks verlieren, doppeln,
     umsortieren, verzögern; 5xx; Timeout nach ausgeführtem Schreibzugriff; Lesestand
     verzögert; Rate Limits; Fremd-Push), CI als Skript mit Flake-Rate.
  2. `localgit`: echte Bare-Repos auf Platte, CI als lokales Skript.
  3. `forgejo`: echtes Forgejo in Docker Compose, echte Webhooks, ohne GitHub-Kosten.
  4. `github`: GitHub App.
- **Webhooks sind Hinweise, der Abgleich ist die Wahrheit.** Jeder Handler ist idempotent;
  Zustand wird vom Forge neu gelesen; Abgleich beim Start und periodisch.
- **Ein Schreiber pro Repo und Branch**, Landen nur als Compare-and-Swap auf die Base
  (erwarteter alter SHA, nie `force`).
- **Journal vor jedem Seiteneffekt**, Wiederanlauf aus Journal plus Forge-Zustand.
- **Speicher:** SQLite (WAL) im Volume; Postgres erst, wenn nötig.
- **Kein git-CLI im Container.** Queue-Branches und Merges laufen serverseitig über die
  Forge-API (Ref anlegen, Merge in den Queue-Branch, Entwurfs-PR). Gelandet wird per Merge-API
  mit erwartetem Head-SHA; danach wird geprüft, dass der Tree des gelandeten Commits dem
  getesteten Tree entspricht. Für `localgit` und Tests reicht `gix` bzw. das git-CLI der
  Testumgebung.

**Invarianten (werden nach jedem simulierten Schritt geprüft)**
- **I0:** Jeder Commit auf der Base hat einen Tree, der einem Commit mit grünen Pflicht-Checks
  entspricht, getestet auf der damaligen Base.
  - *Bündel (Mergifys Semantik, nachgetragen):* Bei `squash` ist jeder Commit eines Bündels der
    getestete Präfix-Tree des Bündel-Branches; Checks liefen nur auf dessen Spitze. Bei
    Bündelgröße 1 gilt I0 streng pro Commit (der Checker hat dafür einen eigenen Modus).
  - *„Grün“ ist nie leer:* Nennen die `merge_conditions` keinen Check, muss der Tree trotzdem
    als Car mit Entwurfs-PR auf dem Queue-Branch (oder in place) bestanden haben, und die
    Bedingungen müssen dort direkt vor dem Landen gegolten haben.
  - *`reset_on_external_merge: never`:* Nach einem Fremd-Commit ist der gelandete Commit die
    getestete Änderung auf dem tatsächlichen Elternteil (Dreiwege-Merge), nicht der getestete
    Tree selbst; der Nutzer verzichtet damit ausdrücklich aufs Neutesten.
  - *Merge-Methoden (P2, nachgetragen):* `merge` und `fast-forward` (Bündel-Branch): jeder
    Commit der First-Parent-Linie ist der getestete Präfix-Tree (wie `squash`); die Commits der
    PRs bleiben als zweite Eltern erhalten. `rebase` und `fast-forward` in place: der letzte
    Commit eines PRs hat den getesteten Tree, seine früheren (nachgespielten bzw. eigenen)
    Commits bilden eine Kette auf dem erwarteten Elternteil, ihre Trees wurden nicht eigens
    getestet. `merge-batch`: ein Merge-Commit pro Bündel, dessen Tree der getestete
    Bündel-Tree ist.
  - *Modi `parallel` und `isolated` (P2):* Ein Car, das nicht auf einem gerade gelandeten Car
    aufbaut, landet wie bei `never` als getestete Änderung auf dem tatsächlichen Elternteil;
    dazwischen dürfen nur Landungen anderer Spuren liegen (`isolated`: jede; `parallel`: nur
    in Scopes, die das Car nicht berührt), Fremd-Commits nur mit `never`.
  - *`skip_intermediate_results` (P2):* Ein grünes Car steht für die Cars davor ein; es gilt
    die Bündel-Semantik, auch bei Bündelgröße 1.
  - *I1 (P2):* Keine Landung, während die Queue pausiert ist, ein Freeze den PR hält oder eine
    `merge_protections`-Regel nicht erfüllt ist; Pflicht-Checks des Rulesets zählen zu den
    Bedingungen.
- **I1:** Kein PR landet, dessen Bedingungen zum Landezeitpunkt falsch sind, dessen Head nicht
  der beim Bau des Cars aufgezeichnete getestete Head ist, oder der schon gelandet ist (nichts
  landet zweimal, auch nicht über Abstürze hinweg). Voraussetzung: Der Prüf-Read vor dem Landen
  ist konsistent und frisch (sonst liest die Engine neu).
- **I2:** Kein PR geht verloren: Jeder eingereihte PR wird gemerged oder mit Grund ausgetragen
  (Lebendigkeit).
- **I3:** Nach Ruhe gilt: temporäre Branches = genutzte Branches. (Ohne Forge-Fehler auch nach
  jedem Schritt; mit Forge-Fehlern oder Abstürzen nach jedem Schritt nur: alles auf dem Forge
  kennt die Engine.)
- **I4:** Laufende Checks ≤ `max_parallel_checks`; Slots werden auf jedem Ausgang frei. Ein
  Car zählt, bis der Forge das Schließen bestätigt; CI auf geschlossenen Entwürfen zählt, bis
  sie abgebrochen ist.
- **I5:** Mit perfekter und mit gestörter Zustellung (Webhooks, Forge-Antworten, Abstürze) ist
  der Endzustand nach dem Abgleich gleich, Dequeue-Gründe eingeschlossen.

## 7. Sprache, Bibliotheken, Container

**Empfehlung: Rust.** Go ist ebenso tragfähig; die Entscheidung liegt bei dir.

| | Rust | Go |
|---|---|---|
| Validator | `jsonschema` 0.60, `serde_yaml_ng`, `fancy-regex` 0.20 (alle im Prototyp geprüft) | `santhosh-tekuri/jsonschema` / `google/jsonschema-go`, `goccy/go-yaml`, `regexp2` |
| GitHub | `octocrab` (etwas Versions-Churn) | `go-github` v93, `go-githubapp` (ausgereifter) |
| MCP | offizielles `rmcp` 3.8 | offizielles `go-sdk` 1.8 |
| Simulation, Modellprüfung | reine Zustandsmaschine, `proptest`, `stateright`, deterministische Ausführung | `rapid`, eigene deterministische Ausführung |
| Vorbilder | rust-lang/bors (bestes Test-Harness mit simuliertem GitHub) | uber/submitqueue (Provider-Leiter fake → git → github), Mic92/gitea-mq, Prow |
| Image | statisch mit musl und rustls, sehr klein | `CGO_ENABLED=0`, klein, Binary meist etwas größer |
| Agenten-Schleife | längere Kompilierzeiten | schnellere Kompilierung |

Für Rust sprechen das starke Typsystem für eine Zustandsmaschine mit vielen Grenzfällen, das
kleinste Image und der funktionierende Prototyp. Für Go sprechen das ausgereiftere GitHub-SDK
und kürzere Agentenschleifen.

**Container (Fokus Größe und Härtung)**
- Mehrstufiger Build, Ziel `x86_64-` und `aarch64-unknown-linux-musl`, rustls statt OpenSSL,
  SQLite gebündelt.
- Laufzeit-Image `FROM scratch` (oder `distroless/static:nonroot`): nur Binary,
  CA-Zertifikate und ein `passwd`-Eintrag für den Nutzer 65532. Keine Shell, kein
  Paketmanager, kein git.
- Betrieb: `read_only: true`, `cap_drop: [ALL]`, `no-new-privileges`, nur `/data` beschreibbar.
  Der Healthcheck ruft `schlange doctor` auf (kein curl nötig).
- CI: Größenbudget als Test (Ziel unter 15 MB; der Wert wird nach dem ersten Build
  festgelegt), Trivy-Scan ohne Befund, SBOM, Signatur mit cosign, reproduzierbarer Build.

## 8. Agent-First-Oberfläche (eigene Features `S-`)

| ID | Feature |
|---|---|
| S-CLI | Jede Funktion per CLI, `--json` überall, stabile Exit-Codes, keine interaktiven Abfragen |
| S-EXPLAIN | `schlange pr explain <n>`: warum ein PR (nicht) eingereiht/gemerged wird, je Bedingung mit Wert und Quelle |
| S-SIM | `schlange sim run <szenario.yaml>`: Szenarien im simulierten Forge, Ausgabe Ereignisprotokoll plus Invariantenbericht |
| S-SIMULATE | `schlange config simulate --pr <n>`: neue Konfiguration gegen echte PRs auswerten, ohne zu handeln |
| S-MCP | MCP-Server (stdio und HTTP). Tools: `queue_status`, `pr_explain`, `config_validate`, `config_simulate`, `events`, `enqueue` und `dequeue` (mit denselben Rechten wie `commands_restrictions`) |
| S-SHADOW | Schattenbetrieb: entscheidet wie im echten Betrieb, handelt nicht, protokolliert Entscheidungen zum Vergleich mit Mergify |
| S-AUDIT | Je Merge ein Nachweis (getesteter SHA und Tree, Base, PR-Heads, gelandeter SHA), abrufbar per CLI und MCP |
| S-DOCTOR | `schlange doctor`: Version, Konfiguration, App-Rechte, Ruleset-Ausnahme, Webhook-Zustellung, Rate Limit |

## 9. Tests

Die Problemfälle sind in [docs/research/merge-queue-probleme.md](docs/research/merge-queue-probleme.md)
katalogisiert (MQ-01 … MQ-53), jeweils mit Szenario, erwartetem Verhalten und Testebene. Jede
ID wird ein Szenario oder ein Property-Test, und die Feature-Datei verweist darauf.

| Ebene | Inhalt |
|---|---|
| Unit | Bedingungsparser, Check-Semantik als Tabellentest, Commit-Nachrichten als Golden Files, Dequeue-Gründe |
| Zustandsmaschine | Property-Tests über zufällige Ereignisfolgen, Invarianten I0–I5; Modellprüfung des Lande-Protokolls |
| Simulierter Forge | Szenarien mit Fehlerinjektion und Absturz an jedem Seiteneffekt, virtuelle Uhr |
| Echte Repos | `localgit` mit Fixture-Repo (Kompilierfehler als „semantischer Konflikt“), alle Merge-Methoden × Bündelgröße 1–5 gegen I0 |
| Forgejo | echte Webhooks und echte Merges in Docker Compose |
| GitHub-Sandbox | kleiner E2E-Satz für Event-Semantik (Queue-PR löst `pull_request` aus, Rulesets, Merge-Status) |
| Differenztest | Mergify und Schlange (Schattenbetrieb) auf demselben Sandbox-Repo und später auf Arztagent; Entscheidungen und Dequeue-Gründe vergleichen. Das ist der Kompatibilitätsnachweis |

## 10. Arbeitsweise: Implementation Agent und Kritiker

- **Verification Skill `verify-schlange`** nach `create-verification-skill`:
  - Launch: Binary bauen bzw. Image bauen.
  - Doctor: `schlange doctor`.
  - Drive: `schlange sim run`, `localgit`- und Forgejo-Szenarien.
  - Evidence: Ereignisprotokoll, Invariantenbericht, `git log` der Base, Audit-Nachweise.
  - Cleanup: nur selbst gestartete Instanzen.

  Die Feature-Liste aus Abschnitt 4 ist die Feature-Map dieses Skills.
- **Implementation Agent:**
  1. Nimmt das nächste offene Feature nach Priorität.
  2. Schreibt zuerst Szenario und Tests aus der Feature-Datei und dem Problemkatalog.
  3. Implementiert.
  4. Belegt mit `verify-schlange`.
  5. Hängt den Nachweis an den PR.
- **Kritiker:** eigene Sitzung mit frischem Kontext, ohne Schreibrechte auf den Code, gern ein
  anderes Modell (z. B. Codex über `codex-dispatch`), damit er nicht dieselben blinden Flecken
  hat.
  - Prüft gegen die Mergify-Doku und das Schema: Verhalten, Defaults, Fehlermeldungen,
    Dequeue-Gründe.
  - Führt die Szenarien selbst aus.
  - Bewertet je Unterfeature: konform, abweichend (mit Beleg) oder fehlt.
  - Prüft, ob die Tests die Problemfälle der Feature-Datei abdecken.
- **Abnahme:** Ein Feature gilt erst als konform, wenn der Nachweis grün ist und der Kritiker
  „konform“ meldet. Der Status steht in der Feature-Datei.

## 11. Phasen

| Phase | Inhalt | Abnahme |
|---|---|---|
| 0 Arbeitsumgebung | Repo-Gerüst, `AGENTS.md`, `verify-schlange` mit simuliertem Forge und Szenarioformat, CI, Container-Gerüst mit Größenbudget | Skill einmal von Anfang bis Ende ausgeführt |
| 0b Feature-Repository | Agent erzeugt Inventar und Feature-Dateien aus Schema, OpenAPI und Doku (P1 = Arztagent), dazu die `S-` Features und den Problemkatalog als Testverweise | Abdeckungstest grün: jeder Inventar-Eintrag ist zugeordnet |
| 1 Arztagent-Parität | Validator, Bedingungen, Queue (seriell, Batch, Bisektion, Two-Step mit Queue-PRs), Befehle, GitHub-Adapter, Schattenbetrieb auf Arztagent | 2 Wochen Schattenbetrieb ohne abweichende Entscheidung, dann Umstieg (Mergify bleibt Rückfall) |
| 2 Volle Parität | restliche Features nach Nutzen | Kritiker: alle Features konform oder bewusst ausgelassen |
| 3 Mergify schlagen | nach [docs/research/mergify-schwaechen.md](docs/research/mergify-schwaechen.md) (W-01 … W-26). W-05 und W-08 kommen schon in Phase 1 | messbare Kriterien je Punkt, z. B. p99 Check → Merge < 10 s, Image < 15 MB, nur Egress zu `api.github.com` |

## 12. Offene Entscheidungen

1. Rust oder Go (Empfehlung Rust).
2. Feature-Liste im selben Repo (`features/`, Empfehlung) oder als eigenes Repository.
3. Kritiker mit anderem Modell (Codex) oder Claude mit frischem Kontext.
4. GitHub-Sandbox-Organisation für E2E- und Differenztests anlegen (braucht einen Menschen:
   GitHub App registrieren, Mergify dort installieren).
