# Merge-Queue-Probleme: Katalog für Tests

Stand 27.10.2026. Jede ID wird mindestens ein Szenario oder Property-Test; die Feature-Dateien
verweisen auf die IDs. [U] = nicht verifiziert.

**Testebenen:**
- **U**: Unit.
- **SM**: Zustandsmaschine bzw. Modell.
- **SF**: simulierter Forge mit Fehlerinjektion und virtueller Uhr.
- **RR**: echte Repos (`localgit`, Forgejo, GitHub-Sandbox).
- **PBT**: Property-basiert.
- **MC**: Modellprüfung.

Invarianten I0 bis I5 siehe [KONZEPT.md](../../KONZEPT.md), Abschnitt 6.

## 1. Korrektheit

| ID | Problem | Erwartet | Test | Beleg |
|---|---|---|---|---|
| MQ-01 | Semantischer Konflikt: A benennt `foo()` um, B ruft `foo()` neu auf; beide einzeln grün, zusammen rot | B wird auf A getestet und fällt dort durch | RR (Kompilier-Check), PBT mit Konflikt-Orakel | Prow Tide Doku |
| MQ-02 | Gelandeter ≠ getesteter Commit: GitHub-Queue am 23.04.2026, 2.092 PRs bei Squash-Bündeln falsch | Getesteten Tree landen bzw. nach dem Landen Tree-Gleichheit prüfen, sonst Queue anhalten | RR, alle Merge-Methoden × Bündel 1–5 gegen I0; PBT | github.blog 2026-04-28, githubstatus zsg1lk7w13cf |
| MQ-03 | Squash/Rebase eines Bündels wendet Patches auf wanderndem Parent erneut an | Commits je PR mit Ziel-Tree beim Testen bauen; I0 je Commit, nicht nur an der Spitze | RR | forum.bors.tech/t/349 |
| MQ-04 | Fast-forward: rebaster Head ist nicht erreichbar, GitHub zeigt „closed“ statt „merged“ | Echten Status melden, sonst schließen mit Kommentar | SF + RR | Mergify Changelog 2026-09-29 |
| MQ-05 | Mehrere Fast-forward-Einträge schnell hintereinander: PR als merged markiert, Commit fehlt | „Merged“ erst nach Erreichbarkeitsprüfung | SM + SF, MC | gitlab#535930 |
| MQ-06 | Base bewegt sich außerhalb der Queue (Direkt-Push, Admin, anderer Merger) | Spekulation auf altem Stand verwerfen, neu aufbauen; Landen nur per CAS | SF mit Push zwischen „grün“ und „Ref setzen“; MC | Mergify lifecycle, direct-merge |
| MQ-07 | PR-Head wird während der Queue gepusht; Webhook geht verloren | Austragen bzw. neu testen; jeder Übergang an Head-SHA gebunden | SF mit verlorenem `synchronize` + Abgleich | homu „approves the wrong commit“ |
| MQ-08 | Jemand pusht auf `mergify/merge-queue/*` | Alle betroffenen PRs austragen, Fehler melden | SF | Mergify rules (`allow_queue_branch_edit`) |
| MQ-09 | PR geschlossen, wieder geöffnet, zum Entwurf gemacht oder von Hand gemerged | Austragen, Abhängige neu bauen; Handmerge = MQ-06 | SM, PBT über Ereignisfolgen | Mergify lifecycle; Details [U] |
| MQ-10 | Base-Branch des PR wird geändert | Aus alter Queue entfernen, gegen Regeln der neuen Base prüfen | SF | [U] |
| MQ-11 | Freigabe entzogen, Label `halt` gesetzt, Bedingung fällt während der CI | Austragen; Bedingungen direkt vor dem Landen erneut prüfen | SM, PBT (I1) | Mergify lifecycle |
| MQ-12 | Flatternde Bedingungen (Label an/aus, Check pending/success) | Keine Doppeleinträge, kein Landen auf kurzzeitigem „wahr“ | SF, Fuzz des Ereignisstroms | Prow Tide |
| MQ-13 | Pflicht-Checks in Branch-Schutz/Ruleset ändern sich mitten im Lauf | Schutz neu lesen (Events `branch_protection_rule`, `repository_ruleset`), sauber reagieren | SF | Mergify Changelog 2026-06/10 |
| MQ-14 | Veraltetes Check-Ergebnis für alten SHA kommt verspätet an | Ergebnisse nach (SHA, Name, App) schlüsseln, fremde SHAs ignorieren | U + SF | Mergify `check-stale` |
| MQ-15 | Check mit gleichem Namen von anderer App oder auf falschem SHA | Quelle per App-ID prüfen (`@app/check`) | SF | GitHub Troubleshooting required checks |
| MQ-16 | neutral/skipped als Erfolg gewertet | Exakt wie Mergify: `check-success` nur `success` | U Tabellentest aller Conclusions × Bedingungen | Mergify conditions |
| MQ-17 | Check-Rerun (rot→grün, grün→rot) | Neuester Lauf je (SHA, Name) gewinnt; ausgetragener PR wird nicht wiederbelebt | SF | Mergify lifecycle |
| MQ-18 | Welche Events lösen Queue-PRs aus (`pull_request`, `push`, nicht `merge_group`) | Exakt wie Mergify, sonst laufen `ci.yml`/`pr-gate.yml` nicht | RR GitHub-Sandbox | GitHub Merge-Queue-Doku, community#15254 |
| MQ-19 | Konfigurationsänderung im PR selbst | Konfig nur von der Base; Queue nach Merge neu bewerten; zwei Konfig-Dateien → Fehler | SF + RR | Mergify file-format |
| MQ-20 | Commit-Nachricht beim Squash/Bündel falsch | Golden Files je Methode | U | Mergify Changelog |

## 2. Bündel und Spekulation

| ID | Problem | Erwartet | Test | Beleg |
|---|---|---|---|---|
| MQ-21 | Bisektion eines roten Bündels: endet nicht, testet Grünes doppelt, Interaktionsfehler zweier PRs | Terminiert in Schranke; kein Unschuldiger fliegt bei einzelnem deterministischem Schuldigen; bei Interaktion fliegt mindestens einer | SM, PBT mit Fehlerprädikat über Teilmengen | Mergify rules (`batch_max_failure_resolution_attempts`) |
| MQ-22 | Flaky Test sprengt ein Bündel | Ohne Retries landet nie ein deterministisch roter PR; Durchsatz messen | Simulation mit Flake-Rate | Trunk pending failure depth, Aviator |
| MQ-23 | Kaskade: A fällt, B/C/D bauen auf A | Jeder laufende Build hat als Vorfahren genau Queue-Präfix + aktuelle Base, sonst verwerfen | SM + MC | Zuul gating; Zuul-Bugfix c945500 |
| MQ-24 | Priorität drängelt vor | Verdrängte Builds abbrechen/ignorieren, Grund protokollieren; I0 | SM, PBT | Mergify Changelog 2026-04-21 |
| MQ-25 | `max_parallel_checks`: Slots lecken nach Abbruch, Off-by-one | I4 | SM | Mergify Changelog |
| MQ-26 | `batch_size`/`batch_max_wait_time`: Timer, Änderung während des Laufs, PR wartet ewig unter Minimum | wie Mergify | SM mit virtueller Uhr | Mergify rules |
| MQ-27 | Konflikt mit PR davor | Halten; verlässt der vordere die Queue, weiter; wird er gemerged, austragen | RR + SM | Mergify lifecycle |
| MQ-28 | CI-Kosten durch Spekulation | CI-Läufe je gelandetem PR als Budget | Simulation | Uber blog, arXiv 2501.03440 |

## 3. Nebenläufigkeit und verteilte Systeme

| ID | Problem | Erwartet | Test | Beleg |
|---|---|---|---|---|
| MQ-29 | Webhooks doppelt, vertauscht, verloren (GitHub liefert nicht automatisch nach) | Handler idempotent, Zustand vom Forge; I5 | SF mit Zustell-Shim, PBT | GitHub failed deliveries |
| MQ-30 | Ohne Webhooks | Queue macht über den Abgleich trotzdem Fortschritt | SF ohne Webhooks | Prow Tide |
| MQ-31 | Absturz mitten im Merge (nach Push, vor Zustand; nach Merge, vor Invalidierung) | Journal vor Seiteneffekt; Wiederanlauf aus Journal + Forge | Deterministische Simulation mit Absturz an jedem Seiteneffekt; MC | bors-ng Forum 430, 275 |
| MQ-32 | Zwei Instanzen / Split Brain | Ein Schreiber je (Repo, Branch), Lease + Fencing, CAS auf Ref | SF mit zwei Schedulern; MC | gitlab#584920, #627754 |
| MQ-33 | Eventual Consistency: `mergeable` unknown, frisch angelegter Branch „existiert nicht“ | Begrenzte Wiederholung mit Backoff; nie austragen wegen Read-after-write | SF mit Lese-Verzögerung | status.mergify.com 7z0fd6335xj3 |
| MQ-34 | Rate Limits und sekundäre Limits (80 Inhalte/min, 500/h) | Zentraler Token-Bucket, `Retry-After`; Landen vor Kosmetik | SF mit 403/429 | GitHub REST rate limits |
| MQ-35 | 5xx/Timeout, obwohl der Merge passiert ist | Nach unklarem Fehler Zustand lesen, nie doppelt landen; Kosmetik separat | SF „Lügen“-Modus | Mergify-Störungen 2026-07/08/09 |
| MQ-36 | Leichen: temporäre Branches und Entwurfs-PRs | GC mit strengem Namensmuster, nie Nutzer-Branches; I3 | SF | Mergify lifecycle |

## 4. CI

| ID | Problem | Erwartet | Test | Beleg |
|---|---|---|---|---|
| MQ-37 | Check meldet sich nie (fehlender Trigger, Pfadfilter, umbenannt, Runner-Limit) | Timeout mit Liste der fehlenden Checks, Hinweis auf Namensabweichung | SF | GitHub Troubleshooting, bors-ng#1506 |
| MQ-38 | `checks_timeout: auto` (p95 + Marge, min. 60 s, erst ab ca. 20 Läufen in 7 Tagen) | Gleicher Algorithmus | U + SM | Mergify Changelog 2026-06 |
| MQ-39 | Abgebrochene Läufe | `checks-interrupted`; bei gleichzeitigem Fehler `checks-failed`; überholte Builds abbrechen, späte Ergebnisse ignorieren | SF | Mergify lifecycle, gitlab#517148 |
| MQ-40 | Forge berechnet Merge-Ref neu (`refs/pull/N/merge`) | Eigene, eingefrorene Test-Refs | RR + SF | gitlab#12562 |
| MQ-41 | CI auf temporären Branches löst Fremdes aus (Deploy) / Kostenexplosion | Dokumentieren; CI-Läufe je PR als Budget | Simulation | Graphite, bors-ng `staging.tmp` |
| MQ-42 | Two-Step-CI: `merge_conditions`-Checks laufen auf dem Queue-PR nicht | Erkennen und als Timeout mit Grund melden | SF | Mergify rules |

## 5. Bedienung und Betrieb

| ID | Problem | Erwartet | Test | Beleg |
|---|---|---|---|---|
| MQ-43 | Queue hängt ohne Grund | Hänger-Erkennung; jeder PR erklärbar (`pr explain`); I2 | MC (Lebendigkeit) | community#15254 |
| MQ-44 | Kopfblockade durch flaky oder langsamen PR | Messen; später isolierter Modus | Simulation | Mergify Changelog 2026-06 |
| MQ-45 | Unklare Dequeue-Gründe | Exakte Mergify-Codes und Meldungen | Golden je Austragungspfad | Mergify data-types (Codes im Schema) |
| MQ-46 | Freeze/Pause mitten im Landen; Ansturm nach Aufheben | Atomar landen oder gar nicht | SM | Mergify pause |
| MQ-47 | Notfall-Bypass (Hotfix direkt auf `main`) | Wie MQ-06 | SF | Trunk, GitHub-Bypass |
| MQ-48 | Monorepo-Scopes falsch berechnet | Überlappende Scopes nie unabhängig getestet | PBT | Aviator affected targets |
| MQ-49 | Gestapelte PRs nach Squash-Merge des unteren | Diffs der oberen bleiben korrekt | RR mit 3er-Stack | Graphite |

## 6. Sicherheit

| ID | Problem | Erwartet | Test | Beleg |
|---|---|---|---|---|
| MQ-50 | Wer darf Befehle auslösen | `commands_restrictions` und Defaults exakt; Recht beim Ausführen prüfen; Befehl muss den Kommentar beginnen | U + Fuzz des Parsers (Unicode, Groß/klein, Zeilenumbrüche), SF-Rechtematrix | Mergify commands/restrictions |
| MQ-51 | Fork-PRs und Tokens | Minimale App-Rechte; Fork-Verhalten wie Mergify | RR Sandbox | GitHub fork workflows; Teile [U] |
| MQ-52 | Bösartige Konfig im PR-Head lockert Bedingungen | Wirkt erst nach Merge; `extends`-Quellen prüfen | SF | Mergify file-format |
| MQ-53 | Ungeprüfter Inhalt landet über den Queue-Branch | Standardmäßig ablehnen (MQ-08) | SF | Mergify rules |

## Testtechniken der Vorbilder

- **Zuul:** In-Process-Fakes der Forges, Ereignisse eingespielt wie vom echten System.
- **rust-lang/bors:**
  - simuliertes GitHub über `wiremock` mit Zustandsmodell im Speicher;
  - signierte Webhooks durch den echten Router;
  - Tester-DSL, gemockte Zeit, `insta`-Snapshots;
  - Staging-Repo plus Seed-Skript.
- **uber/submitqueue:** Provider-Leiter `fake` → `git` (Bare-Repo) → `github`.
- **Mic92/gitea-mq:** E2E gegen echtes Gitea plus Postgres, dazu ein HTTP-Fake für GitHub.
- **Prow:** `fakegithub`, `localgit`, Fake-Server für Integrationstests.
- **trunk-io/mergequeue-tool:** Last- und Flake-Simulator gegen echte Repos.
- **Für Schlange:**
  - simulierter Forge mit Fehlerinjektion;
  - deterministische Simulation mit Absturzpunkten;
  - Modellprüfung des Lande-Protokolls;
  - PBT mit echtem git und I0 in jedem Schritt;
  - kleine GitHub-Sandbox;
  - Differenztest gegen Mergify.
