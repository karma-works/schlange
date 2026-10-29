# Sandbox

GitHub sandbox for E2E and differential tests. Guide: [docs/betrieb/sandbox.md](../docs/betrieb/sandbox.md),
shadow mode on Arztagent: [docs/betrieb/schattenbetrieb.md](../docs/betrieb/schattenbetrieb.md).

| Path | Content |
|---|---|
| `setup.sh` | check the organisation, smee channel, apps, repos, fixture, rulesets |
| `apps/` | app manifests and one-click registration (`create_app.py`) |
| `fixture/` | content of the sandbox repos: imitation of the Arztagent checks and `.mergify.yml` |
| `driver/sandbox.py` | replays scenarios and records the behaviour of the queue |
| `scenarios/` | scenarios S01–S17 with references to the problem catalogue |
