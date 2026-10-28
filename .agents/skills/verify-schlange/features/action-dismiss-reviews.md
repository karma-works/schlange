---
id: action-dismiss-reviews
title: "Action `dismiss_reviews`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:dismiss_reviews"
  - "config:pull_request_rules.actions.dismiss_reviews"
  - "config:pull_request_rules.actions.dismiss_reviews.*"
  - "config:defaults.actions.dismiss_reviews"
  - "config:defaults.actions.dismiss_reviews.*"
  - "default:pull_request_rules.actions.dismiss_reviews.*"
  - "default:defaults.actions.dismiss_reviews.*"
  - "enum:when:*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/dismiss-reviews"
---

# Action `dismiss_reviews`

Dismisses approvals and change requests. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. With `when: synchronize` (default) it dismisses the reviews given before a new head that the actions did not push themselves; with `always`, every matching review on every evaluation.
- `options`: built: `approved`, `changes_requested` (`true`, `false`, a list of logins, `from_requested_reviewers`), `message`, `when`.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-people.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- The GitHub adapter reads reviews only when a rule needs them (review, dismiss_reviews, request_reviews, comment, close) or a trailer does.
