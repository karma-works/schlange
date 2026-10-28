---
id: action-edit
title: "Action `edit`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:edit"
  - "config:pull_request_rules.actions.edit"
  - "config:pull_request_rules.actions.edit.*"
  - "config:defaults.actions.edit"
  - "config:defaults.actions.edit.*"
  - "default:pull_request_rules.actions.edit.*"
  - "default:defaults.actions.edit.*"
unsupported:
  - "config:pull_request_rules.actions.edit.bot_account"
  - "config:defaults.actions.edit.bot_account"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/edit"
---

# Action `edit`

Turns a pull request into a draft or back. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated.
- `options`: `draft` built (on GitHub through GraphQL `convertPullRequestToDraft` / `markPullRequestReadyForReview`); `bot_account` not built.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-people.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- A draft leaves the queue: the queue conditions usually ask `-draft`.
