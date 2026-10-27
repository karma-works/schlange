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
- `actions`: only `queue` is built; every other action is flagged through its own feature (`action-*`).
- `description`: accepted; it has no behaviour.
- `queue-prs-excluded`: built. Draft pull requests of the queue are never evaluated against rules.
- `disabled`: built. A rule with `disabled` (and its `reason`) does not run. The docs only say the key holds the reason; that a disabled rule does not run is our reading of the name.

## How to get to it (user POV)

- A rule applies → the pull request is queued; report in the queue's check.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Both Arztagent rules are evaluated.

## Gotchas

- Some actions are edge-triggered, some level-triggered; `queue` is level-triggered.
