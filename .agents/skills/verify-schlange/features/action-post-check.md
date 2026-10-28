---
id: action-post-check
title: "Action `post_check`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:post_check"
  - "config:pull_request_rules.actions.post_check"
  - "config:pull_request_rules.actions.post_check.*"
  - "config:defaults.actions.post_check"
  - "config:defaults.actions.post_check.*"
  - "default:pull_request_rules.actions.post_check.*"
  - "default:defaults.actions.post_check.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/post-check"
---

# Action `post_check`

Reports a check run with a title and a summary (deprecated in Mergify). Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. Our reading of the docs: without `success_conditions` and `neutral_conditions` the check is posted on every open pull request, success when the rule matches and failure otherwise; with either set, only where the rule matches.
- `options`: built: `title` and `summary` as legacy templates (Schlange's Jinja2 subset: expressions with filters, `if`/`elif`/`else`, `for`; variables `check_rule_name`, `check_status`, `check_succeeded`, `check_succeed`, `check_conditions` and pull request attributes), `success_conditions`, `neutral_conditions`. The check is named `Rule: <rule> (post_check)`. Default title and summary are Schlange's own.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-people.yaml`.
- **Engine.** `cargo test -p schlange-core --test actions`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- The app's own check runs are not read back as conditions (cosmetics rule): a rule cannot condition on a post_check result.
- A template the subset cannot render posts the check with the error as its summary.
