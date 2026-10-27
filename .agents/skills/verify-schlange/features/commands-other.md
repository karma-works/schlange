---
id: commands-other
title: "Commands `backport`, `copy`, `rebase`, `squash`, `update`"
priority: P2
arztagent: false
status: missing
inventory:
  - "cmd:backport"
  - "cmd:copy"
  - "cmd:rebase"
  - "cmd:squash"
  - "cmd:update"
problems:
  - "MQ-50"
source: "https://docs.mergify.com/commands"
---

# Commands `backport`, `copy`, `rebase`, `squash`, `update`

Further comment commands that trigger the actions of the same name once.

## Sub-features

- `backport`
- `copy`
- `rebase`
- `squash`
- `update`

## How to get to it (user POV)

- Comment on the pull request.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Permissions, see `commands-restrictions`.
