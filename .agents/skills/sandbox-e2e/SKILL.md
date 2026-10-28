---
name: sandbox-e2e
description: "Run end-to-end and differential tests against real GitHub in the sandbox organisation schlange-sandbox: record Mergify's behaviour on queue-lab, run Schlange on schlange-e2e (or in shadow mode next to Mergify), compare outcomes against baselines and capture evidence. Use when a feature file says a behaviour 'needs a sandbox' or must be 'confirmed on the sandbox', when Mergify's real behaviour is needed for a baseline, or when someone asks for an end-to-end or integration test on GitHub."
---

# Sandbox end-to-end tests

The fake GitHub (`crates/schlange-github-fake`) proves the engine; the sandbox proves what only
real GitHub can tell: which events a queue draft triggers, real status codes, ruleset bypass,
read-after-write lag, and what Mergify actually does with the same pull requests. It is the one
exception to AGENTS.md rule 7. Stay inside it.

Setup, identities and the fixture are described in
[docs/betrieb/sandbox.md](../../../docs/betrieb/sandbox.md). Paths below are relative to the
repository root; `$D` is `python3 -I sandbox/driver/sandbox.py`.

## 0. Rules that are not negotiable

- **Only `schlange-sandbox`.** Never another repository, never Arztagent. The credentials only
  work there; do not look for others.
- **One run per sandbox repository at a time.** Two scenarios in one repository land in the same
  batch and spoil each other. The driver waits until no `sbx/*` or queue pull request is open;
  the workflow has a concurrency group. Never pass `--no-wait` while someone else may be running.
- **Clean room.** `trace.json` and `summary.json` contain Mergify's comment texts: never commit
  them, never paste them into feature files. Commit only `outcome.json` (our own format) as a
  baseline, and write findings in our own words.
- **`cargo test` stays offline.** Sandbox runs are evidence, not unit tests.

## 1. Choose where to run

| Where | When | Credentials |
|---|---|---|
| GitHub Actions (preferred for agents) | recording Mergify, regression against baselines | repository secrets, set by `sandbox/ci/upload-secrets.sh` |
| Local | debugging, full traces, running `schlange serve` against `schlange-e2e` | `~/.config/schlange-sandbox/` (apps, `human-token`, `mergify-api-key`) |

Actions:

```bash
gh workflow run sandbox.yml -f repo=queue-lab -f scenarios="s01-single-pr s03-red-batch"
run=$(gh run list -w sandbox.yml -L1 --json databaseId --jq '.[0].databaseId')
gh run watch "$run" --exit-status
gh run download "$run" -D evidence/sandbox-$run      # outcome.json, run.json, scenario.json
```

`scenarios=all` runs every scenario. The job summary has one row per scenario. A Schlange
compatibility run fails before starting if any selected scenario lacks a reviewed baseline;
record new baselines on `queue-lab` first.

Local:

```bash
bash sandbox/setup.sh check                     # org, apps, installations, rulesets, keys
$D run sandbox/scenarios/s01-single-pr.json --repo queue-lab
```

## 2. Pick the scenario

| Question from a feature file | Scenario | Repository |
|---|---|---|
| Does a queue draft trigger `pull_request` workflows (MQ-18), two-step CI (MQ-42) | S01, S02 | both |
| Batch formation and squash messages (MQ-20, MQ-26) | S02 | both |
| Bisection of a red batch, who is blamed (MQ-21, MQ-23) | S03, S04 | both |
| Dequeue on `halt`, new push, draft (MQ-07, MQ-09, MQ-11) | S05, S06, S07 | both |
| `commands_restrictions`, constitution paths (MQ-50, MQ-52) | S08, S11 | both |
| Checks that never pass, missing check (MQ-16, MQ-37) | S09 | both |
| Base moves outside the queue, conflicts (MQ-06, MQ-27, MQ-47) | S10, S12, S13 | both |
| Same-name Actions checks and manual custom-review retry (MQ-14, MQ-15) | S14, S15 | both; record reference first |

No scenario fits? Write one (section 6) before changing code: the scenario is the test.

## 3. Record Mergify (queue-lab)

Mergify is the reference on `queue-lab`. Run the scenario there, then read
`.sandbox-runs/<run>/outcome.json` (locally also `trace.json` for the timeline and Mergify's
activity log, `mergify_events`).

To make a run a baseline: check that it shows the behaviour the scenario is about (not a
timeout, not a half-enabled Mergify), then

```bash
cp .sandbox-runs/<run>/outcome.json sandbox/baselines/<scenario-file-name>.json
```

and commit it with a sentence on what it shows. From then on every run of that scenario, on
either repository, is compared against it, and the weekly workflow run notices when Mergify
changes.

## 4. Run Schlange

**Active, on `schlange-e2e`** (local; the workflow does not start `serve` yet):

```bash
cargo build --release -p schlange
set -a; . ~/.config/schlange-sandbox/sandbox.env; set +a
npx smee-client -u "$SMEE_URL" -t http://127.0.0.1:8080/webhook &      # webhooks of the queue app
target/release/schlange serve \
  --app-id "$(jq -r .id ~/.config/schlange-sandbox/apps/queue.json)" \
  --private-key ~/.config/schlange-sandbox/apps/queue.pem \
  --webhook-secret-file ~/.config/schlange-sandbox/apps/queue.webhook-secret \
  --repo schlange-sandbox/schlange-e2e --data-dir "$(mktemp -d)" --listen 127.0.0.1:8080 &
target/release/schlange doctor          # must show no FAIL before driving
$D run sandbox/scenarios/<scenario>.json --repo schlange-e2e --baseline sandbox/baselines/<scenario>.json
```

The fixture exempts only `mergify[bot]` from its PR gate, as Arztagent currently does. Schlange
drafts take the fixture's heading check; its simplified gate does not run Arztagent's classifier.

**Shadow, on `queue-lab`**, next to Mergify: the same `serve` with `--shadow` and
`--repo schlange-sandbox/queue-lab`, then `$D run … --repo queue-lab` and
`target/release/schlange shadow report --repo schlange-sandbox/queue-lab --data-dir <dir>`.
Mergify acts, Schlange only decides; the report lists where they differ. This is the rehearsal
for shadow mode on Arztagent ([docs/betrieb/schattenbetrieb.md](../../../docs/betrieb/schattenbetrieb.md)).

Stop everything you started (`serve`, `smee-client`) when done.

## 5. Evidence

A sandbox claim is proven by:

- the command and its exit code (`compare`: 0 equal, 1 differences, printed line by line);
- `outcome.json` of the run and the baseline it was compared with;
- for Actions, the run URL; locally, the run directory name;
- for shadow mode, the `shadow report` output.

Put them under `evidence/<run>/` like `verify-schlange` does, and name the feature ID. In the
feature file, replace "needs a sandbox" with what the run showed, in our own words, and point to
the scenario. A difference to Mergify is a finding: reproduce it as a scenario in the simulated
forge (`scenarios/`) with a test before fixing it (KONZEPT.md, rule 1).

## 6. Writing a scenario

`sandbox/scenarios/sNN-<name>.json`, steps in order (full list in the docstring of
`sandbox/driver/sandbox.py`):

```json
{"id": "S12", "title": "…", "refs": ["MQ-xx"], "steps": [
  {"open": "a", "flags": ["fail-full"]},
  {"open": "b", "as": "human", "conflict": true},
  {"comment": "a", "text": "@mergifyio queue", "as": "human"},
  {"label": "a", "add": "halt", "as": "human"},
  {"push": "a"}, {"ready": "a"}, {"draft": "a"}, {"main_commit": {"shared/x.txt": "…{run}…"}},
  {"wait": "done", "prs": ["a", "b"], "timeout": 2400},
  {"wait": "quiet", "seconds": 300, "timeout": 1800}
]}
```

CI behaviour comes from flags the pull request itself changes (`sandbox/fixture/ci/run.sh`):
`fail`, `fail-light`, `fail-full`, `conflicts-with <key>`, `flaky <percent>`, `sleep <s>`,
`sleep-full <s>`. Actors: `driver` (the driver app's bot, not allowed to send commands) and
`human` (the organisation-scoped token, the allowed sender). End every scenario with a `wait`;
the driver closes the scenario's pull requests after recording unless `--keep` is given.

S14 and S15 use a human token authorized to dispatch workflows. S14 verifies the Actions job's
check association, workflow, event and head. S15's synthetic manual result uses a custom check:
its run link is supplied by the app and is only a hint, even after the linked run is validated.

## 7. Cleanup and troubleshooting

- `$D reset --repo <repo>` closes open scenario pull requests and deletes `sbx/*` branches.
- `bash sandbox/setup.sh fixture` rewrites the fixture on both `main` branches (after changing
  `sandbox/fixture/`).
- Mergify does nothing although its simulator says the rule matches: the products **Merge Queue**
  and **Workflow Automation** must both be on for `queue-lab` in Mergify's dashboard; pull
  requests opened before that need `@mergifyio refresh`.
- `wait_idle` gives up after 30 minutes: something is still open. Look before you reset; it may
  be another agent's run.
- A scenario that hits its timeout still records; treat that outcome as unproven, never as a
  baseline.
