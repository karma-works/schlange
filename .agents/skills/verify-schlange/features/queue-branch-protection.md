---
id: queue-branch-protection
title: "Required checks from branch protection and rulesets"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.branch_protection_injection_mode"
  - "default:queue_rules.branch_protection_injection_mode"
  - "enum:branch_protection_injection_mode:*"
problems:
  - "MQ-13"
  - "MQ-15"
  - "W-05"
  - "W-11"
source: "https://docs.mergify.com/merge-queue/github-rulesets"
---

# Required checks from branch protection and rulesets

The queue takes the checks that branch protection or rulesets of the base require into its own
conditions. By default they become part of the queue conditions. In Arztagent the ruleset on
`main` requires the check `Risk class and data protection` (PR gate).

## Sub-features

- `mode-queue`: built. Default: each required check joins the queue conditions and the merge conditions as typed `check-success` conditions (`gates::queue_conditions`, `gates::merge_conditions`), so a pull request whose required check fails never enters the queue, and a car lands only when the required checks passed on its draft.
- `mode-merge`, `mode-none`: built. `merge`: only the merge conditions; `none`: nothing is injected (Mergify then needs a `merge_bot_account` that satisfies the protection itself; Schlange does not check that).
- `read`: built. `schlange serve` reads the `required_status_checks` rules that apply to every base with a queue and to the default branch (`GET /rules/branches/{branch}`, readable with `metadata: read`) at start and on every reconciliation, and tells the engine when they change (`Event::RequiredChecks`). Classic branch protection is not read (it needs `administration: read`, which Schlange does not ask for).
- `read-fresh`: built in the fake. `repository_ruleset` and `branch_protection_rule` webhooks trigger immediate reconciliation. Required checks are read while the durable identity hold is still in force, before a fresh green snapshot or recovered landing may produce decisions. A failed protection read keeps that hold and retries within 60 seconds (MQ-13).
- `self-block`: built. The queue's own check (`--check-name`) is never taken over, so the queue never waits for itself (W-05).
- `app-pinning`: built in the model and fake. Optional numeric `integration_id` survives the REST read, engine event and journal; injected atoms require a current successful check run with exactly that publishing app ID. Other apps, unknown app IDs and commit statuses cannot satisfy a pin. Unpinned contexts retain name matching (MQ-15).
- `I1`: the invariant checker injects the same typed checks (scenario field `required_checks`, a string or `{context, integration_id}`) and checks every landing against them. Scenario CI accepts `app_id` as the publishing integration.

## How to get to it (user POV)

- Ruleset in the repository settings.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Simulation.** Run `target/release/schlange sim run scenarios/p2-required-checks.yaml`: `main` requires `lint`; #1 is enqueued only after `lint` passed (+0:02, not at +0:01 when `quick` passed) and lands, #2 with a failing `lint` never enters. Exit 0.
- **Pinned simulation.** Run `target/release/schlange sim run scenarios/p2-required-checks-app-pin.yaml`: a success from another integration cannot hide a failing pinned run.
- **Freshness regressions.** `cargo test -p schlange-github-fake --test e2e_p2` covers both protection webhook names and a newly required check observed alongside an already-green draft.
- **serve against the fake GitHub.** `cargo test -p schlange-github-fake --test e2e_p2 required_checks_of_a_ruleset_join_the_queue_conditions` (the queue's own check in the ruleset is left out).
- `target/release/schlange config validate --strict scenarios/configs/two-step.yml` exits 0 (the default injection mode is supported).

## Gotchas

- Classic branch protection remains unread because the app does not request `administration: read`. Ruleset pinning and webhook freshness are covered against the fake; real GitHub parity still needs sandbox evidence.
