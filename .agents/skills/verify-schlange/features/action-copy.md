---
id: action-copy
title: "Action `copy`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:copy"
  - "config:pull_request_rules.actions.copy"
  - "config:pull_request_rules.actions.copy.*"
  - "config:defaults.actions.copy"
  - "config:defaults.actions.copy.*"
  - "default:pull_request_rules.actions.copy.*"
  - "default:defaults.actions.copy.*"
unsupported:
  - "config:pull_request_rules.actions.copy.bot_account"
  - "config:defaults.actions.copy.bot_account"
  - "config:pull_request_rules.actions.copy.merge_conflict_style"
  - "config:defaults.actions.copy.merge_conflict_style"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/copy"
---

# Action `copy`

Copies the pull request onto other branches, also before the merge. Arztagent does not use the action.

## Sub-features

- `run`: built. Edge-triggered (docs: workflow, "How rules are evaluated"): runs once when the rule starts to match, again only after it stopped matching. Unmerged: the pull request's changes against its merge base; merged: the merge commit. Branch `mergify/copy/<branch>/pr-<n>`.
- `options`: as `backport` (see action-backport), same limits.
- `command`: `@mergifyio copy <branches>` built.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-copies.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Crashes.** `cargo test -p schlange-forge --test action_crashes`: a crash before and after every side effect of the action scenarios posts nothing twice and leaves the same labels, assignees, checks and copies.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- The docs' precondition that the pull request merges cleanly with its base is not checked.
