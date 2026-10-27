---
id: conflict-resolvers
title: "Conflict resolvers (experimental)"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:conflict_resolvers"
  - "config:conflict_resolvers.*"
  - "default:conflict_resolvers.*"
  - "enum:strategy:*"
  - "enum:language:*"
  - "enum:package-manager:*"
problems:
  - "MQ-27"
source: "https://docs.mergify.com/merge-queue/batches"
---

# Conflict resolvers (experimental)

While a batch is built, conflicts in named files are resolved with a strategy instead of
dequeueing the pull request.

## Sub-features

- `paths`
- `package-manager`
- `language`

## How to get to it (user POV)

- Section `conflict_resolvers`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Takes effect only when the batch branch itself lands.
