# Mergify: Funktionsumfang (Überblick)

Stand 27.10.2026. In eigenen Worten zusammengefasst. Maßgeblich für die vollständige
Feature-Liste ist das gepinnte Schema (`.research/mergify/schema.json`, JSON Schema 2020-12),
dazu `openapi.json` und die Doku-Seiten im selben Ordner. Neu laden:
`https://docs.mergify.com/llms.txt` (jede Seite gibt es als `.md`),
`…/mergify-configuration-schema.json`, `…/openapi.json`.

Aufwand: S = Tage, M = 1–3 Wochen, L = über einen Monat.

## Umfang in Zahlen

236 Konfigurationspfade, 76 PR-Attribute, 18 Aktionen, ca. 44 Dequeue-Gründe, 79
API-Operationen, 12 Top-Level-Schlüssel: `shared`, `extends`, `defaults`, `scopes`,
`commands_restrictions`, `pull_request_rules`, `queue_rules`, `merge_protections`,
`priority_rules`, `merge_queue`, `merge_protections_settings`, `conflict_resolvers`.

## Bereiche

| Bereich | Inhalt | Aufwand |
|---|---|---|
| Konfiguration | Sechs mögliche Dateinamen, nur vom Default-Branch; `extends` (gleiche Org, eine Ebene), `defaults`, YAML-Anker in `shared`, `disabled` je Regel; Validierung als Check „Configuration changed“ | S–M |
| Bedingungen | `[-][#]attribut [op wert]`; `and`/`or`/`not`; `=`, `!=`, `~=` (Python-Regex), `*=` (Glob), Vergleiche; Listenattribute = „irgendein Element“, außer `!=`; Typen Zeitplan, Zeitstempel, relativ, Dauer, Priorität; `@app/check`; Check-Auswahl: neueste Check-Suite | Parser M, Datenlader für alle Attribute L |
| Merge Queue | mehrere Queues, `queue_conditions`/`merge_conditions` (Two-Step); Modi serial/parallel/isolated; `max_parallel_checks` (Default 5); Batch (`batch_size` 1–128 oder min/max, Wartezeit Default 30 s); Bisektion; `skip_intermediate_results`; `checks_timeout` (auto = p95); `max_checks_retries`; Checks im PR selbst vs Entwurfs-PR `mergify/merge-queue/<10 hex>`; Direct Merge; Merge-Methoden merge/squash/rebase/fast-forward/merge-batch; Update merge/rebase; Commit-Nachrichtenformat; Branch-Schutz-Injektion; Prioritäten; Pause; geplanter Freeze; Labels `queued`/`dequeued`; Status-Kommentare; Check „Mergify Merge Queue“; Scopes für Monorepos; experimentelle Konflikt-Resolver; Stack-Unterstützung; Statistik | L gesamt |
| Workflow-Regeln | `pull_request_rules` zustandsbasiert; Aktionen flanken- oder pegelgesteuert, einige exklusiv; Queue-PRs werden nie ausgewertet. Aktionen: assign, backport, copy, close, comment, delete_head_branch, dismiss_reviews, edit, github_actions, label, merge, post_check, queue, rebase, request_reviews, review, squash, update | M + je Aktion S–M |
| Befehle | `@mergifyio`/`@mergify`, Groß/klein egal, am Kommentaranfang; 👍-Reaktion plus Antwort; queue, dequeue, requeue (Alias), backport, copy, rebase, update, squash, refresh; `commands_restrictions` mit Defaults (`sender-permission >= write`, für rebase/update/squash/refresh auch der Autor eines Fork-PRs); Checkbox-Kommentare | S |
| Merge Protections | eigene Regeln zu einem Check; eingebaut: `Depends-On`, `Merge-After`, Konfig geändert; Freeze-Fenster; Auto-Merge | M |
| CI Insights / Test Engine | Job-Metriken, Auto-Retry (nur GitHub Actions), JUnit-Ingest, Flaky-Erkennung, Quarantäne | L, getrenntes Produkt |
| Stacks | Client-CLI, ein Commit = ein PR über `Change-Id` | L, getrennt |
| API / CLI / Integrationen | REST v1; CLI mit `--json`; Slack, Datadog, Terraform; keine ausgehenden Webhooks | M |
| Dashboard | Queue-Ansicht, Statistik, Konfig-Editor/Simulator, Freezes | L |

## GitHub-spezifisch

- App-Installation und Webhooks; Check-Runs vs. Commit-Status; Deployments.
- Branch-Schutz und Rulesets: Mergify braucht dort eine Ausnahme, „Exempt“-Modus.
- Entwurfs-PRs als Träger der Queue; CODEOWNERS; `github-review-decision`; Teams.
- Dependabot-Attribute.
- App-Rechte:
  - lesen und schreiben: Actions, Checks, Contents, Deployments, Issues, Pages, Pull requests,
    Workflows;
  - nur lesen: Administration, Statuses, Members, Merge queues.

## Lizenz und Clean-Room

- Die Engine ist seit 2022 nicht öffentlich.
- Die Mergify-CLI ist seit dem 08.10.2026 proprietär. Ihre Lizenz verbietet ausdrücklich, sie
  für ein Konkurrenzprodukt einzusetzen.
- Für Schlange heißt das:
  - Verhalten nur aus der öffentlichen Doku, dem Schema und Beobachtung.
  - Kein Code und keine Doku-Texte von Mergify ins Repo.
  - Die Mergify-CLI nicht benutzen.
