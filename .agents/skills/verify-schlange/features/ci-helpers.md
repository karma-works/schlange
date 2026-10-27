---
id: ci-helpers
title: "CI helpers for queue runs"
priority: P2
arztagent: false
status: missing
inventory:
  - "cli:mergify ci git-refs"
  - "cli:mergify ci queue-info"
problems:
  - "MQ-40"
source: "https://docs.mergify.com/cli"
---

# CI helpers for queue runs

Small commands for CI jobs: print the base and head ref of a run and information about the current
queue run.

## Sub-features

- `refs`
- `queue-info`

## How to get to it (user POV)

- A CI job calls the helper.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- In Arztagent, `ci.yml` recognises queue pull requests by branch prefix, not through this helper.
