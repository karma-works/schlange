---
id: config-simulator
title: "Configuration simulator"
priority: P2
arztagent: false
status: missing
inventory:
  - "api:POST /repos/{owner}/{repository}/configuration-simulator"
  - "api:POST /repos/{owner}/{repository}/pulls/{number}/simulator"
  - "api:POST /repos/{owner}/{repository}/simulator"
  - "cli:mergify config simulate"
problems:
  - "W-12"
source: "https://docs.mergify.com/configuration/file-format"
---

# Configuration simulator

Evaluate a configuration against real pull requests without acting. In Schlange this is
`S-SIMULATE`.

## Sub-features

- `against-pr`
- `against-repo`

## How to get to it (user POV)

- `schlange config simulate --pr <n>` (planned).

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Needs the forge adapter.
