---
id: action-close
title: "Action `close`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:close"
  - "config:pull_request_rules.actions.close"
  - "config:pull_request_rules.actions.close.*"
  - "config:defaults.actions.close"
  - "config:defaults.actions.close.*"
  - "default:pull_request_rules.actions.close.*"
  - "default:defaults.actions.close.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/close"
---

# Action `close`

Closes the pull request and posts the message. Arztagent does not use the action.

## Sub-features

- `run`: built. Edge-triggered (docs: workflow, "How rules are evaluated"): runs once when the rule starts to match, again only after it stopped matching. At most once per evaluation: the first matching rule in the file wins (docs: rule syntax).
- `options`: `message` built (message template); the default text is Schlange's own.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Crashes.** `cargo test -p schlange-forge --test action_crashes`: a crash before and after every side effect of the action scenarios posts nothing twice and leaves the same labels, assignees, checks and copies.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- A queued pull request that a rule closes leaves the queue (`PR_DEQUEUED`).
