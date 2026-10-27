---
id: merge-protections
title: "Merge protections and auto-merge"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:merge_protections"
  - "config:merge_protections.*"
  - "config:merge_protections_settings"
  - "config:merge_protections_settings.*"
  - "default:merge_protections_settings.*"
  - "enum:reporting_method:*"
  - "default:merge_protections.*"
problems:
  - "MQ-13"
source: "https://docs.mergify.com/merge-protections"
---

# Merge protections and auto-merge

Custom rules that show up as a check on the pull request and block merging until they hold; plus
auto-merge without a queue.

## Sub-features

- `rule`
- `reporting`
- `auto-merge`

## How to get to it (user POV)

- Section `merge_protections`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Reporting as check run or deployment.
