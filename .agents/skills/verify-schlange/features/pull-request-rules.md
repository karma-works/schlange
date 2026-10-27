---
id: pull-request-rules
title: "Workflow rules (`pull_request_rules`)"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:pull_request_rules"
  - "config:pull_request_rules.name"
  - "config:pull_request_rules.conditions"
  - "config:pull_request_rules.actions"
  - "config:pull_request_rules.description"
  - "config:pull_request_rules.disabled"
  - "config:pull_request_rules.disabled.reason"
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

- `evaluation`: state-based, on every change of the pull request.
- `queue-prs-excluded`: queue pull requests are ignored.
- `disabled`: switch a rule off with a reason (P2).

## How to get to it (user POV)

- A rule applies → the pull request is queued; report in the queue's check.

## Driving it

- `schlange sim run scenarios/p0-queue-conditions.yaml`: both Arztagent rules are evaluated.

## Gotchas

- Some actions are edge-triggered, some level-triggered; `queue` is level-triggered.
