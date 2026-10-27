---
id: conditions-reviews
title: "Review attributes"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:github-review-decision"
  - "attr:github-require-review-from-specific-teams"
  - "attr:github-code-owner-review-satisfied"
  - "attr:github-review-approved"
  - "attr:github-require-last-push-approval"
  - "attr:review-requested"
  - "attr:approved-reviews-by"
  - "attr:dismissed-reviews-by"
  - "attr:changes-requested-reviews-by"
  - "attr:commented-reviews-by"
  - "attr:review-threads-resolved"
  - "attr:review-threads-unresolved"
  - "enum:github-review-decision:*"
problems:
  - "MQ-11"
source: "https://docs.mergify.com/configuration/conditions"
---

# Review attributes

Approvals, change requests, requested reviewers, code owners and review threads as conditions.

## Sub-features

- `approvals`
- `decision`
- `threads`

## How to get to it (user POV)

- Conditions.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Teams and code owners need further app permissions.
