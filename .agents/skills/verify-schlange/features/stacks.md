---
id: stacks
title: "Stacks (stacked pull requests)"
priority: P3
arztagent: false
status: missing
inventory:
  - "cli:mergify stack *"
problems:
  - "MQ-49"
source: "https://docs.mergify.com/stacks"
---

# Stacks (stacked pull requests)

A client CLI that turns a series of commits into one pull request each and keeps them in sync.
Arztagent uses no stacks.

## Sub-features

- `create`
- `push`
- `sync`

## How to get to it (user POV)

- Local CLI.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not planned.** No verification path; the feature stays `missing` on purpose.

## Gotchas

- The Mergify CLI is proprietary and is not used.
