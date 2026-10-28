---
id: action-update
title: "Action `update`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:update"
  - "config:pull_request_rules.actions.update"
  - "config:pull_request_rules.actions.update.*"
  - "config:defaults.actions.update"
  - "config:defaults.actions.update.*"
  - "default:pull_request_rules.actions.update.*"
  - "default:defaults.actions.update.*"
unsupported:
  - "config:pull_request_rules.actions.update.bot_account"
  - "config:defaults.actions.update.bot_account"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/update"
---

# Action `update`

Merges the base into the pull request. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. Only for an open pull request that is not queued and is behind its base (as read, or the base moved since the read); at most once per evaluation.
- `options`: `bot_account` not built.
- `command`: `@mergifyio update` built (answers when up to date, closed or queued).

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml` and `scenarios/p2-actions-copies.yaml` (command).
- **Engine.** `cargo test -p schlange-core --test actions`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP). (`PUT /pulls/{n}/update-branch`)
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- The head the update pushes does not trigger `dismiss_reviews` (`synchronize`).
