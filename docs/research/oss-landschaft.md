# Open-Source-Landschaft Merge Queues

Stand 27.10.2026. Metadaten live über die GitHub-API geprüft; [U] = nicht verifiziert.

| Projekt | Sprache / Lizenz | Status | Kern | Für Schlange |
|---|---|---|---|---|
| rust-lang/bors | Rust, MIT/Apache | sehr aktiv, produktiv bei Rust | axum, sqlx/Postgres, octocrab; Befehle seriell; periodischer Refresh; Zwei-Branch-Trick; nutzt git-CLI | Bestes Rust-Vorbild, vor allem das Test-Harness |
| uber/submitqueue | Go, Apache-2.0 | seit 01/2026, sehr aktiv | Spekulationsbaum (Paper EuroSys'19); Provider `fake`/`git`/`github`; MySQL, Microservices | Provider-Leiter kopieren; selbst zu schwer |
| Mic92/gitea-mq | Go, MIT | 2026, aktiv | Gitea/Forgejo/GitHub; als Pflicht-Status plus Auto-Merge; Bündel mit Bisektionslimit; Webhooks plus Abgleich (30 s aktiv, 15 min ruhig) | Gutes kleines Go-Vorbild; Forgejo in Docker als echter Forge für Tests |
| Prow Tide | Go, Apache-2.0 | aktiv | reine Abgleichsschleife über GraphQL-Suche, Pools je Repo/Branch, ghProxy-Cache | Vorbild für Abgleich und Rate Limits |
| Zuul | Python, Apache-2.0 | aktiv (14.3, 09/2026) | Erfinder des spekulativen Gatings; Fenster wie TCP (+1 bei Erfolg, halbiert bei Fehler); `Depends-On` über Repos | Algorithmus-Vorbild |
| Bulldozer | Go, Apache-2.0 | gepflegt | zustandslos, regelbasiert, keine Queue; `go-githubapp` | Vorbild für App-Gerüst in Go |
| Kodiak | Python, AGPL-3.0 | aktiv | Label-gesteuert, seriell, Rate-Limit-Pacing | Kein Code übernehmen (AGPL) |
| Marge-bot (marge-org) | Python, BSD-3 | aktiv | GitLab, Rebase, seriell | Vorbild GitLab-Adapter |
| bors-ng, homu | Elixir / Python | archiviert | Ursprung „Not Rocket Science“, Bisektion | nur historisch |
| Mergify Engine | Python, Apache-2.0 (alt) | seit 2022 privat; Fork Jsn2win/mergify-engine | – | Clean-Room, alten Code nicht nutzen |
| trunk-io/mergequeue-tool | Rust, MIT | aktiv | Last- und Flake-Simulator gegen echte Repos | Idee für `schlange sim` |
| aviator-co/av, niche-git | Go, MIT | aktiv | Stacked-PR-CLI; Dateiliste ohne Klon | Ideen für Stacks und ohne-git-Betrieb |
| CanadaApollo6/openmerge | Go, Apache-2.0 | seit 15.10.2026 | positioniert sich als „Mergify-Ideen, offen“ | direkter Mitbewerber, beobachten |
| weitere (LumaPrism/mergequeue, goat-merge, fila, brawl, Asika, pushq, jjq) | Rust/Go | klein, unreif | – | höchstens Referenz |

Proprietär zum Vergleich:
- **GitHub Merge Queue:** `merge_group`, `gh-readonly-queue/*`, Gruppen 1–100. Keine Bisektion
  [U], kein Two-Step. Für private Repos nur mit Enterprise Cloud.
- **GitLab Merge Trains:** Premium.
- **Aviator, Trunk, Graphite:** Anti-Flake-Mechanismen „pending failure depth“ bzw. „optimistic
  validation“; Stack-Unterstützung.

## Gelernte Regeln

1. **Not Rocket Science:** Die Base bekommt nur exakt getestete Stände. Nach dem Test nicht neu
   mergen.
2. **Zwei Refs:** zuerst auf einem Ref bauen, das keine CI auslöst, dann auf das CI-Ref
   schieben.
3. Mutationen seriell je Repo/Branch, Ein-/Ausgabe parallel.
4. Webhooks sind Hinweise, der Abgleich ist die Wahrheit. Zustellungs-ID deduplizieren, HMAC
   prüfen.
5. **Rate Limits:** GraphQL bündeln, ETags, Takt je Installation, aktive Queues häufiger
   abfragen.
6. **Spekulation:** kumulative Kette reicht; Baum mit Erfolgsmodell erst bei Monorepo-Größe.
7. **Rotes Bündel:** Bisektion mit Versuchslimit, oder austragen und Rest neu einreihen.
8. **Flaky Tests:** Retries, „spätere grüne Kombination entlastet frühere“, mit bekanntem Risiko.
9. Head-SHA beim Einreihen festhalten, Landen als Compare-and-Swap.
10. Pflicht-Checks aus Branch-Schutz/Rulesets ableiten, eigenen Status als Pflicht-Check.
11. Ist der PR schon aktuell und grün, den Queue-Build überspringen.

## Bibliotheken (geprüft 27.10.2026)

| Zweck | Rust | Go |
|---|---|---|
| JSON Schema | `jsonschema` 0.60.2 (MIT) | `santhosh-tekuri/jsonschema` v6.0.3, `google/jsonschema-go` v0.4.3 (MIT) |
| YAML | `serde_yaml_ng`, `saphyr` (`serde_yaml` archiviert) | `goccy/go-yaml` v1.19 |
| Regex mit Lookahead | `fancy-regex` 0.20 | `dlclark/regexp2` |
| Git | `gix` 0.90 (Merge im Speicher, kein Push), `git2` 0.21 (libgit2) | `go-git` v5 (schwache Merges) |
| GitHub | `octocrab` 0.54 | `go-github` v93, `go-githubapp`, `ghinstallation` |
| MCP | `rmcp` 3.8 (offiziell) | `modelcontextprotocol/go-sdk` 1.8 (offiziell) |
| Sonstiges | CUE v0.17 (Go, von Google entstanden): Konfigurationssprache, importiert JSON Schema | |
