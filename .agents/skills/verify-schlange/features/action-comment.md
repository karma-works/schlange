---
id: action-comment
title: "Action `comment`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:comment"
  - "config:pull_request_rules.actions.comment"
  - "config:pull_request_rules.actions.comment.*"
  - "config:defaults.actions.comment"
  - "config:defaults.actions.comment.*"
  - "default:pull_request_rules.actions.comment.*"
  - "default:defaults.actions.comment.*"
unsupported:
  - "config:pull_request_rules.actions.comment.bot_account"
  - "config:defaults.actions.comment.bot_account"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/comment"
---

# Action `comment`

Posts a comment with a message template. Arztagent does not use the action.

## Sub-features

- `run`: built. Edge-triggered (docs: workflow, "How rules are evaluated"): runs once when the rule starts to match, again only after it stopped matching. Comments on merged or closed pull requests run too (e.g. a `merged` rule), but only on pull requests the engine saw open.
- `options`: `message` built (variable substitution: `author`, `number`, `title`, `body`, `base`, `head`, `labels`, `assignees`, `approvers`, `queue_dequeue_reason`; unknown placeholders stay verbatim). A `comment` without a message (rule or `defaults`) is a configuration error. `bot_account` is not built.
- `idempotence`: every comment carries a hidden marker per rule and firing; the adapters skip a post whose marker is there, so a crash or a lost reply never posts twice.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-label-comment.yaml`.
- **Engine.** `cargo test -p schlange-core --test actions`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Crashes.** `cargo test -p schlange-forge --test action_crashes`: a crash before and after every side effect of the action scenarios posts nothing twice and leaves the same labels, assignees, checks and copies.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- `{{ labels }}` leaves out the queue's own labels (cosmetics are never read back).
