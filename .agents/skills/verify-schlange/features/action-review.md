---
id: action-review
title: "Action `review`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:review"
  - "config:pull_request_rules.actions.review"
  - "config:pull_request_rules.actions.review.*"
  - "config:defaults.actions.review"
  - "config:defaults.actions.review.*"
  - "default:pull_request_rules.actions.review.*"
  - "default:defaults.actions.review.*"
  - "enum:type:*"
unsupported:
  - "config:pull_request_rules.actions.review.bot_account"
  - "config:defaults.actions.review.bot_account"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/review"
---

# Action `review`

Submits a review in the app's name. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. A review is posted when the app's latest review is not of that type with that message (also after it was dismissed).
- `options`: `type` (`APPROVE`, `REQUEST_CHANGES`, `COMMENT`) and `message` (message template) built; a comment or change request without a message gets Schlange's own text. `bot_account` not built.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-people.yaml`: approved once, dismissed by a push, approved again.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- Together with `dismiss_reviews` an approval comes back after every push: that is what the two rules describe.
