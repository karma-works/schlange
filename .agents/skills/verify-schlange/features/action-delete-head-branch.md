---
id: action-delete-head-branch
title: "Action `delete_head_branch` (deprecated)"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:delete_head_branch"
  - "config:pull_request_rules.actions.delete_head_branch"
  - "config:pull_request_rules.actions.delete_head_branch.*"
  - "config:defaults.actions.delete_head_branch"
  - "config:defaults.actions.delete_head_branch.*"
  - "default:pull_request_rules.actions.delete_head_branch.*"
  - "default:defaults.actions.delete_head_branch.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/delete-head-branch"
---

# Action `delete_head_branch` (deprecated)

Deletes the head branch once the pull request is closed (deprecated in Mergify). Arztagent does not use the action.

## Sub-features

- `run`: built. Edge-triggered (docs: workflow, "How rules are evaluated"): runs once when the rule starts to match, again only after it stopped matching. A rule that matches while the pull request is open waits for the close; branches of forks are left alone.
- `options`: `force` built: without it a branch that another open pull request uses as its base is kept.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- GitHub's own "delete head branches" setting does the same; the docs recommend it.
