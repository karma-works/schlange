---
id: action-rebase
title: "Action `rebase`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:rebase"
  - "config:pull_request_rules.actions.rebase"
  - "config:pull_request_rules.actions.rebase.*"
  - "config:defaults.actions.rebase"
  - "config:defaults.actions.rebase.*"
  - "default:pull_request_rules.actions.rebase.*"
  - "default:defaults.actions.rebase.*"
unsupported:
  - "config:pull_request_rules.actions.rebase.bot_account"
  - "config:defaults.actions.rebase.bot_account"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/rebase"
---

# Action `rebase`

Rebases the pull request onto its base. Arztagent does not use the action.

## Sub-features

- `run`: built in the simulated forge. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. Only for an open pull request that is not queued and has no waiting `queue` command that is behind its base and not from a fork.
- `github`: not built: without a git CLI the API cannot replay commits; the adapter refuses with a hint to use `update`.
- `command`: `@mergifyio rebase` built (answers when up to date, a fork, closed or queued).

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml`.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- The non-linear-history trigger (merge commits in the pull request) is not built; only "behind the base" triggers a rebase.
- Never resent after a failure or a restart; the engine decides again on the next read. The new head is a new head like any other: it cancels a waiting `queue` command.
