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

- `paths`: missing.
- `package-manager`: missing.
- `language`: missing.
- `auto`: missing (`auto: true` with optional `directories`, added to the schema in November 2026).

## How to get to it (user POV)

- Section `conflict_resolvers`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Takes effect only when the batch branch itself lands.
- The schema marks the section as experimental, so every explicit `conflict_resolvers`, an empty list included, is reported as unsupported.
- `merge_queue.resolve_conflicts` was removed from the schema in November 2026; a configuration that still sets it is invalid (`unknown key`).
