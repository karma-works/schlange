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

- `mode-queue`: built. Default: each required check joins the queue conditions and the merge conditions as `check-success = <name>` (`gates::queue_conditions`, `gates::merge_conditions`), so a pull request whose required check fails never enters the queue, and a car lands only when the required checks passed on its draft.
- `mode-merge`, `mode-none`: built. `merge`: only the merge conditions; `none`: nothing is injected (Mergify then needs a `merge_bot_account` that satisfies the protection itself; Schlange does not check that).
- `read`: built. `schlange serve` reads the `required_status_checks` rules that apply to every base with a queue and to the default branch (`GET /rules/branches/{branch}`, readable with `metadata: read`) at start and on every reconciliation, and tells the engine when they change (`Event::RequiredChecks`). Classic branch protection is not read (it needs `administration: read`, which Schlange does not ask for).
- `read-fresh`: partial. Ruleset changes take effect at the next reconciliation (`--reconcile-every`), not at once (MQ-13).
- `self-block`: built. The queue's own check (`--check-name`) is never taken over, so the queue never waits for itself (W-05).
- `app-pinning`: missing. A required check pinned to an app (`integration_id`) is matched by name only (MQ-15).
- `I1`: the invariant checker injects the same checks (scenario field `required_checks`) and checks every landing against them.

## How to get to it (user POV)

- Ruleset in the repository settings.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Simulation.** Run `target/release/schlange sim run scenarios/p2-required-checks.yaml`: `main` requires `lint`; #1 is enqueued only after `lint` passed (+0:02, not at +0:01 when `quick` passed) and lands, #2 with a failing `lint` never enters. Exit 0.
- **serve against the fake GitHub.** `cargo test -p schlange-github-fake --test e2e_p2 required_checks_of_a_ruleset_join_the_queue_conditions` (the queue's own check in the ruleset is left out).
- `target/release/schlange config validate --strict scenarios/configs/two-step.yml` exits 0 (the default injection mode is supported).

## Gotchas

- Check names can come from two apps; rulesets can pin an app (MQ-15): not distinguished.
