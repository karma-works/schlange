---
id: action-squash
title: "Action `squash`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:squash"
  - "config:pull_request_rules.actions.squash"
  - "config:pull_request_rules.actions.squash.*"
  - "config:defaults.actions.squash"
  - "config:defaults.actions.squash.*"
  - "default:pull_request_rules.actions.squash.*"
  - "default:defaults.actions.squash.*"
  - "enum:commit_message:*"
unsupported:
  - "config:pull_request_rules.actions.squash.bot_account"
  - "config:defaults.actions.squash.bot_account"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/squash"
---

# Action `squash`

Squashes the pull request's commits into one. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. Only for an open pull request that is not queued and has no waiting `queue` command with more than one commit.
- `options`: `commit_message` built (`all-commits`: title with the number and one line per commit; `first-commit`; `title+body`).
- `command`: `@mergifyio squash [mode]` built.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml`.
- **Engine.** `cargo test -p schlange-core --test actions`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP). (git data API: one commit with the head's tree on the merge base)
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- On GitHub a fork's branch is refused.
- Never resent after a failure or a restart; the engine decides again on the next read. The new head is a new head like any other: it cancels a waiting `queue` command.
