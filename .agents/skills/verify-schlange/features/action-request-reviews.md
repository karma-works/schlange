---
id: action-request-reviews
title: "Action `request_reviews`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:request_reviews"
  - "config:pull_request_rules.actions.request_reviews"
  - "config:pull_request_rules.actions.request_reviews.*"
  - "config:defaults.actions.request_reviews"
  - "config:defaults.actions.request_reviews.*"
  - "default:pull_request_rules.actions.request_reviews.*"
  - "default:defaults.actions.request_reviews.*"
unsupported:
  - "config:pull_request_rules.actions.request_reviews.bot_account"
  - "config:defaults.actions.request_reviews.bot_account"
  - "config:pull_request_rules.actions.request_reviews.users_from_teams"
  - "config:defaults.actions.request_reviews.users_from_teams"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/request-reviews"
---

# Action `request_reviews`

Requests reviews from users and teams. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated.
- `options`: `users`, `teams`, `random_count` with weights built; the author is never requested; users already requested or who reviewed (dismissed reviews included) and teams already requested are skipped and count toward `random_count`; at most 15 requests per pull request, users first. The random choice is deterministic per pull request and rule (weighted, seeded by the hash of both), so the pure core stays reproducible.
- `users_from_teams`: not built (needs team membership reads).

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-people.yaml`.
- **Engine.** `cargo test -p schlange-core --test actions`. (random choice)
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- Reviewer permissions are not checked (GitHub refuses logins without access).
