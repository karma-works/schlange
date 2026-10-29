---
id: action-github-actions
title: "Action `github_actions` (deprecated)"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:github_actions"
  - "config:pull_request_rules.actions.github_actions"
  - "config:pull_request_rules.actions.github_actions.*"
  - "config:defaults.actions.github_actions"
  - "config:defaults.actions.github_actions.*"
  - "default:pull_request_rules.actions.github_actions.*"
  - "default:defaults.actions.github_actions.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/github-actions"
---

# Action `github_actions` (deprecated)

Starts GitHub Actions workflows by dispatch (deprecated in Mergify). Arztagent does not use the action.

## Sub-features

- `run`: partial. Edge-triggered (docs: workflow, "How rules are evaluated"): runs when the rule starts to match, again only after it stopped matching. An isolated simulator regression found that a crash after the decision checkpoint but before its intent is recorded loses an unsent dispatch; the paired post-send case sends it only once. The recovery fix and GitHub/SQLite evidence remain outstanding.
- `options`: built: `workflow.dispatch` with `workflow` (file name or id), `ref` (default: the default branch), `inputs` (templates with `author`, `base`, `head`, `number`, `repository_full_name`, `title`; numbers and booleans as text).

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP). (`POST /actions/workflows/{id}/dispatches`)

## Gotchas

- A dispatch cannot be made idempotent: after a crash in doubt it is not sent again (at most once).
