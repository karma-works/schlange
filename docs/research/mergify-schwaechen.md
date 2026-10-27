# Mergify-Schwächen und wie Schlange sie schlägt (Phase 3)

Stand 27.10.2026. Quellen:
- Issues und Diskussionen in github.com/Mergifyio/mergify (Nummern unten);
- docs.mergify.com/changelog, /security, /cli/agents;
- mergify.com/pricing, enterprise.mergify.com/requirements, status.mergify.com.

Schwere gilt für ein kleines privates Team auf GitHub mit GitHub Actions.
[U] = nicht verifiziert. [W] = Werbeaussage eines Mitbewerbers.

Bewertungen von G2/Capterra waren nicht abrufbar. Die Statusseite ist lückenhaft: In ihrem
Feed fehlen 2024 bis Juli 2026, und #5177 nennt Hänger, die dort nie auftauchten.

| ID | Schwäche | Beleg | Schwere | Schlange | Messgröße |
|---|---|---|---|---|---|
| W-01 | Stille Hänger über Stunden | #5177 (08/2026), #5179 (~8,5 h), #5180 (4,5 h), #5164 (~23 h, nur Free-Tier) | H | Abgleichsschleife plus Webhooks, Watchdog „Check fertig, Queue bewegt sich nicht“ | Chaos-Test: Erholung in N Abgleichsintervallen ohne Mensch |
| W-02 | Latenz durch Ereignisrückstau | #5181 (25–60+ min), #5183 (30–45 min, Befehle bis 118 min) | H | Ein Mandant, `check_suite`/`check_run` direkt verarbeiten | p99 „letzter Pflicht-Check → Merge“ < 10 s, „Kommentar → Bestätigung“ < 5 s |
| W-03 | Statusseite unvollständig | Feed, #5177 | M | eigene Metriken, `/health`, lokales Störungsprotokoll | jeder Hänger > 5 min in `schlange events` und Metriken |
| W-04 | Free-Tier zweitklassig | #5164; 24 h Datenhaltung | M | keine Tarife | – |
| W-05 | Selbstblockade: eigener Check als Pflicht-Check im Ruleset, aus dem die Queue ihre Bedingungen ableitet | #5177, #5181 | H | eigenen Check nie im eigenen Gate verlangen; Zyklenprüfung beim Laden | Unit-Test: Selbstabhängigkeit wird mit klarer Meldung abgelehnt |
| W-06 | Veraltete Caches, Umbenennungen | #5180, #5178, #5139, #5131 | M | Identität per Node-/User-ID statt Login; Rulesets frisch lesen (ETag) | Umbenennungstests (Konto, App, Workflow) |
| W-07 | Regressionen der Auswertung ohne Konfig-Änderung | #5165, #5175, #5174 | M | Auswerter versioniert; Golden-Korpus echter PR-Schnappschüsse in CI | null Entscheidungsdiffs zwischen Releases |
| W-08 | PR fliegt nach eigenem Rebase raus | #5135 (offen seit 03/2025) | M | selbst gepushte SHAs merken, per Tree vergleichen | E2E: Rebase-Update, PR bleibt in der Queue |
| W-09 | Ständige Abkündigungen (~20 in 2024–2026) | Changelog: Partitionen, `commit_message_template`, `autoqueue`, Jinja2 ab 31.10.2026, `allow_queue_branch_edit`, `github_actions` | H | stabile, versionierte Konfig; `schlange migrate`; alte Versionen ≥ 2 Jahre | Breaking Changes pro Jahr außerhalb Major = 0 |
| W-10 | Entfernte Features brechen Bestandsnutzer | Badge-API 08/2026, Datadog-Metriken 09/2026, #5167 | M | Warnung im PR-Summary plus Auto-Fix-PR | Kompatibilitätssuite historischer Konfigurationen |
| W-11 | Komplexe, teils implizite Konfiguration (Ableitung aus Rulesets) | #5175, #5177, #5183 | M | `schlange config explain`: wirksame Regeln inkl. Ruleset-Herkunft | Zeit bis zum ersten Merge |
| W-12 | Validierung nur online/über proprietäre CLI; Schema-Cache-Fehler | #5161, cli/agents | L–M | offline `validate` und `simulate`, Schema im Binary | validate < 200 ms offline |
| W-13 | „Warum merged mein PR nicht?“ („under evaluation“) | #5181, #5183, #5139 | H | `schlange pr explain` als Ursachenbaum mit Konfig-Zeile, Wert, Zeitstempel | kein wartender PR ohne maschinenlesbaren Grund |
| W-14 | Unnötige Entwurfs-PRs und CI-Lärm | #5145, #5139 | M | nur so viele Queue-PRs wie nötig; Titel kompatibel mit Titel-Checks | zusätzliche PRs/Läufe je Merge |
| W-15 | CI-Kosten | Trunk-Vergleich [W] | M | Direct Merge, Retry vor Austragen, Bisektion mit gecachten Ergebnissen | CI-Minuten je gemergtem PR auf wiedergespielter Historie |
| W-16 | Fork-PRs | Changelog 2026-09-29, #5136 | L | klare Diagnose, Rückfall auf Merge-Update | – |
| W-17 | Closed Source, CLI mit Wettbewerbsverbot | Diskussion #5035; mergify-cli LICENSE 08.10.2026 | H | OSI-Lizenz, reproduzierbare Builds, öffentlicher Testkorpus | – |
| W-18 | Lock-in | Zustand in Mergifys Redis | M | Zustand in SQLite beim Nutzer, `schlange export` | Arztagent-Konfig läuft ohne Änderung |
| W-19 | Self-Hosting nur Enterprise (Postgres, Redis mit 16 DBs, 2 vCPU/2 GB) | enterprise.mergify.com | M | ein statisches Binary, SQLite | Installation bis erster Merge < 10 min, RAM im Leerlauf < 100 MB, Image < 15 MB |
| W-20 | Kosten 21 $/Mitwirkendem und Monat (Max) | Pricing | M | 0 $ Lizenz | Betriebskosten je Monat |
| W-21 | Datenschutz: Hosting-Region nicht angegeben, AVV nur auf Anfrage, Unterauftragnehmer nur im Trust Center | /security, Pricing | H (Gesundheitsumfeld) | eigene Infrastruktur, keine Telemetrie | Egress-Allowlist-Test: nur `api.github.com` |
| W-22 | Breite App-Rechte (15, u. a. Contents/Workflows/Actions write, Administration read) | /security | H | minimale Rechte, Zusatzrechte nur je Modul | Rechteanzahl; Test: fehlendes Recht → saubere Meldung |
| W-23 | Bot-Konten-Reibung | #5153, #5074 | L | als App handeln, Nutzer-Token optional | Dependabot/Renovate-E2E ohne Bot-Konfig |
| W-24 | Abhängig von GitHub-Störungen (404-Sturm 28.05.2026, Verzögerungen 18.08.2026) | Statusseite | M | Backoff mit Jitter, 404/5xx vorläufig, Vollabgleich nach Störung | injizierte 404/5xx-Stürme: kein falsches Austragen |
| W-25 | Kein MCP; Agent Skills nur über proprietäre CLI mit Token | /cli/agents, #5166 | M | MCP-first, `--json` mit stabilem Schema, idempotente Befehle | Agent erledigt Aufgaben ohne HTML-Scraping; Vertragstests |
| W-26 | Nur GitHub; Support 1 Werktag, 24/7 kostet [W] | Trunk | L | Forge-Abstraktion (Forgejo, später GitLab) | Adapter-Testsuite je Forge |

## Die zehn wirksamsten Punkte

1. `schlange pr explain` als Ursachenbaum (W-13).
2. Webhooks plus Abgleich, Hänger-Watchdog, SLO-Metriken (W-01, W-03, W-24).
3. Unter 10 s von „Check fertig“ bis Merge, gemessen und veröffentlicht (W-02).
4. Offline `validate`/`simulate` plus Golden-Korpus (W-07, W-12).
5. Stabile, versionierte Konfig, `migrate`, Import aus `.mergify.yml` (W-09, W-10, W-18).
6. Minimale App-Rechte (W-22).
7. Kein Datenabfluss, ein Binary mit SQLite (W-19, W-21).
8. MCP und `--json`-Vertrag (W-25).
9. ID-basierte Identität, frische Rulesets, Zyklenprüfung (W-05, W-06, W-11).
10. Weniger CI-Minuten (W-14, W-15).

## Folgen schon für Phase 1

- W-05 und W-08 sind Fehler, die Arztagent mit der eigenen Konfiguration treffen kann. Sie
  gehören als Tests in Phase 1, auch wenn das Verhalten dabei von Mergify abweicht. Die
  Abweichung wird in der Feature-Datei als „bewusst besser“ dokumentiert.
- W-01, W-02 und W-24 messen wir im Schattenbetrieb schon gegen Mergify mit.
