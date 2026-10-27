---
id: scopes
title: "Scopes for monorepos"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:scopes"
  - "config:scopes.*"
  - "default:scopes.*"
  - "attr:scope"
  - "cli:mergify ci scopes"
  - "cli:mergify ci scopes-send"
  - "api:POST /repos/{owner}/{repository}/pulls/{number}/scopes"
  - "api:PUT /repos/{owner}/{repository}/commits/{sha}/scopes"
  - "api:PUT /repos/{owner}/{repository}/pulls/{number}/scopes"
problems:
  - "MQ-48"
source: "https://docs.mergify.com/merge-queue/scopes"
---

# Scopes for monorepos

Pull requests are grouped by the parts of a monorepo they touch; independent parts can be tested
in parallel.

## Sub-features

- `files`
- `manual`
- `barrier`
- `capacity`

## How to get to it (user POV)

- Section `scopes`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Overlapping scopes (MQ-48).
