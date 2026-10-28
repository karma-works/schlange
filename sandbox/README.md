# Sandbox

GitHub-Sandbox für E2E- und Differenztests. Anleitung: [docs/betrieb/sandbox.md](../docs/betrieb/sandbox.md),
Schattenbetrieb auf Arztagent: [docs/betrieb/schattenbetrieb.md](../docs/betrieb/schattenbetrieb.md).

| Pfad | Inhalt |
|---|---|
| `setup.sh` | Organisation prüfen, smee-Kanal, Apps, Repos, Fixture, Rulesets |
| `apps/` | App-Manifeste und Registrierung mit einem Klick (`create_app.py`) |
| `fixture/` | Inhalt der Sandbox-Repos: Nachbau der Arztagent-Checks und `.mergify.yml` |
| `driver/sandbox.py` | spielt Szenarien ab und zeichnet das Verhalten der Queue auf |
| `scenarios/` | Szenarien S01–S11 mit Verweisen auf den Problemkatalog |
