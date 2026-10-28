# Schattenbetrieb auf Arztagent

- **Ziel:** Schlange läuft neben Mergify auf `karma-works/arzt-agent` und trifft dieselben
  Entscheidungen, ohne zu handeln (KONZEPT.md, Feature S-SHADOW, Phase 1).
- **Abnahme:** zwei Wochen ohne abweichende Entscheidung, danach der Umstieg.
- **Voraussetzung:** S-SHADOW ist gebaut (`schlange serve --shadow`, `schlange shadow report`,
  siehe AGENTS.md). Vor Arztagent muss es in der Sandbox (`queue-lab`, neben Mergify) dieselben
  Entscheidungen treffen; die offenen Punkte stehen in
  `.agents/skills/verify-schlange/features/S-SHADOW.md` („confirmed on the sandbox“).

## Grundsatz: Schatten durch Rechte, nicht durch ein Flag

Für den Schatten gibt es eine eigene App, „Schlange Schatten Arztagent“
([Manifest](../../sandbox/apps/manifests/shadow.json)), die **nur Leserechte** hat: metadata,
contents, pull requests, issues, checks und statuses, alle `read`. Damit kann Schlange auf
Arztagent technisch weder mergen noch kommentieren noch Branches anlegen, auch wenn der Code
einen Fehler hat oder versehentlich ohne `--shadow` startet.

Im Arztagent-Repo ändert sich nichts: `.mergify.yml`, `pr-gate.yml`, die Rulesets und Mergify
bleiben unverändert. Das Risiko für den laufenden Betrieb ist null.

## Einrichtung

| Schritt | Wer |
|---|---|
| 1. App registrieren: `python3 -I sandbox/apps/create_app.py shadow --user karma-works --webhook-url <url>`, im Browser „Create GitHub App“ | du (ein Klick) |
| 2. App nur auf `karma-works/arzt-agent` installieren (Link gibt das Skript aus) | du (Admin-Zustimmung) |
| 3. Mergify-API-Key für Arztagent anlegen (Dashboard → API keys), Leserechte genügen | du |
| 4. Instanz auf eigener Infrastruktur (VPS) einrichten: Container, Volume `/data`, App-Schlüssel als Secret, Webhook-URL der App auf die Instanz | Agent, nach deiner Freigabe des Hosts |
| 5. `schlange serve --shadow --mergify-login mergify[bot]` starten, `schlange doctor` muss grün sein (Rechte, Webhook-Zustellung, Rate Limit) | Agent |
| 6. Täglicher Bericht `schlange shadow report`, Abweichungen als Issue im Schlange-Repo | Agent |

Arztagent ist ein medizinisches Projekt. Die PR-Daten liegen deshalb nur auf eigener
Infrastruktur und nicht bei einem Drittanbieter (kein smee.io im Dauerbetrieb).

## Was verglichen wird

Schlange trifft seine Entscheidungen und liest gleichzeitig, was Mergify tatsächlich getan hat.
Quellen dafür sind die Queue-PRs von `mergify[bot]`, die Merges, die Kommentare und Check-Runs
von Mergify und die Ereignisse der Mergify-API.

| Ebene | Vergleich |
|---|---|
| Einreihen | je PR: eingereiht ja/nein, welche Regel, welche Bedingung fehlte |
| Austragen | Dequeue-Grund je PR |
| Befehle | `@mergifyio queue`/`dequeue`: angenommen oder abgelehnt (`commands_restrictions`) |
| Bisektion | welcher PR als Schuldiger gilt, welche PRs trotzdem landen |
| Landen | Reihenfolge und Zusammensetzung der Bündel (tolerant: Bündel hängen vom Timing ab), Squash-Commit-Nachricht |

**Grenze:** Im Schatten legt Schlange keine eigenen Queue-PRs an. Für die Merge-Bedingungen
übernimmt es die CI-Ergebnisse von Mergifys Queue-PRs als Eingabe. Damit wird die
Entscheidungslogik geprüft, nicht das eigene Spekulieren. Das prüft die E2E-Sandbox
(`schlange-e2e`).

Eine Abweichung zählt erst als behoben, wenn sie als Szenario im simulierten Forge reproduziert
und mit Test behoben ist (KONZEPT.md, Regel 1).

## Umstieg nach der Abnahme

1. Die Schatten-App bekommt Schreibrechte (contents, pull requests, issues, checks, workflows:
   write). GitHub fragt dich als Admin nach Zustimmung.
2. Verfassungs-PR durch einen Menschen: In `pr-gate.yml` wird der Bot-Login der Schlange-App
   akzeptiert (heute fest `mergify[bot]`).
3. Ruleset-Ausnahme für die Schlange-App auf `main`, per Skript nach dem Muster von
   `scripts/mergify-setup.sh setup`.
4. Mergify pausieren, nicht deinstallieren. Es bleibt der Rückfallweg, bis Schlange einige Wochen
   stabil landet.

## Zwischenschritt (optional, vor S-SHADOW)

Die Leserechte-App kann schon vorher installiert werden und nur Ereignisse aufzeichnen
(Webhooks plus Abgleich). Daraus entstehen echte Abläufe von Arztagent, die man im simulierten
Forge erneut abspielen kann. Ein Risiko für Arztagent entsteht dabei nicht, weil die App nur
lesen kann.
