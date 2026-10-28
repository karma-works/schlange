---
id: action-assign
title: "Action `assign`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:assign"
  - "config:pull_request_rules.actions.assign"
  - "config:pull_request_rules.actions.assign.*"
  - "config:defaults.actions.assign"
  - "config:defaults.actions.assign.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/assign"
---

# Action `assign`

Assigns or unassigns people. Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated.
- `options`: built: `add_users`, `users` (the older form, merged into `add_users`), `remove_users`; `{{ author }}` is filled in; logins ending in `[bot]` are skipped when adding; a login in both lists is left alone.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-people.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Crashes.** `cargo test -p schlange-forge --test action_crashes`: a crash before and after every side effect of the action scenarios posts nothing twice and leaves the same labels, assignees, checks and copies.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- Only open pull requests are assigned.
