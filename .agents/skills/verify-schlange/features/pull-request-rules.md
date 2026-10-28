---
id: pull-request-rules
title: "Workflow rules (`pull_request_rules`)"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:pull_request_rules"
  - "config:pull_request_rules.name"
  - "config:pull_request_rules.conditions"
  - "config:pull_request_rules.actions"
  - "config:pull_request_rules.description"
  - "config:pull_request_rules.disabled"
  - "config:pull_request_rules.disabled.reason"
  - "default:pull_request_rules.description"
problems:
  - "MQ-12"
  - "MQ-19"
source: "https://docs.mergify.com/workflow/rule-syntax"
---

# Workflow rules (`pull_request_rules`)

Rules consist of conditions and actions. When the conditions hold for a pull request, the actions
run. Queue pull requests are never evaluated against rules. Arztagent has two rules, both with the
action `queue`.

## Sub-features

- `evaluation`: built. State-based: `name`, `conditions` and `actions` are parsed and the conditions evaluated after every event (`crates/schlange-config/src/rules.rs`).
- `conditions-required`: built. `conditions` is required (docs: rule syntax): an empty list or `true` matches every pull request; a rule without the key or without a value is a configuration error in `Rules` too, so `serve`, which reads rules without the schema stage, no longer treats it as "match every pull request" (critic 1abc; `cargo test -p schlange-config pull_request_rule_conditions_are_required`).
- `actions`: built: `queue` and every other action (`crates/schlange-core/src/actions.rs`); each action's options and limits are in its own feature file (`action-*`).
- `edge-and-level`: built. Edge-triggered actions (`comment`, `close`, `delete_head_branch`, `merge`, `copy`, `backport`, `github_actions`) run once per match and again after the rule stopped matching; level-triggered ones (`assign`, `dismiss_reviews`, `edit`, `label`, `post_check`, `rebase`, `request_reviews`, `review`, `squash`, `update`) hold the pull request in the state the rule describes and never repeat an outstanding request (docs: workflow, "How rules are evaluated").
- `combining`: built. Every matching rule runs its actions; `close`, `delete_head_branch`, `merge`, `rebase`, `squash` and `update` run at most once per evaluation, the first matching rule in the file wins (docs: rule syntax, "Combining rules").
- `queue-interplay`: built. Actions run after the queue's own decisions in the same step; their changes (labels, drafts, closes, pushes) are read back like any other change and can queue or dequeue a pull request. `update`, `rebase` and `squash` leave queued pull requests alone. Rules run on merged and closed pull requests only when the engine saw them open.
- `crash-safety`: built. Every action request is journaled before it is sent; edge requests stay pending until answered and are resent after a restart or a lost reply, idempotent at the forge (markers on comments and reviews, set operations, head-bound rewrites, reused copy branches); workflow dispatches at most once.
- `description`: accepted; it has no behaviour.
- `queue-prs-excluded`: built. Draft pull requests of the queue are never evaluated against rules.
- `disabled`: built. A rule with `disabled` (and its `reason`) does not run. The docs only say the key holds the reason; that a disabled rule does not run is our reading of the name.

## How to get to it (user POV)

- A rule applies → the pull request is queued; report in the queue's check.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Both Arztagent rules are evaluated.
- **Scenario.** `scenarios/p2-actions-*.yaml` run every action next to the queue.

## Gotchas

- Some actions are edge-triggered, some level-triggered; `queue` is level-triggered.
- Rules that add and remove the same label: within one evaluation the first rule wins; after ten reversals within an hour the actions stop changing the label (see action-label).
