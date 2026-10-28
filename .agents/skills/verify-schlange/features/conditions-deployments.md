---
id: conditions-deployments
title: "Deployment attributes"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:deployment-success"
  - "attr:deployment-failure"
problems:
  - "MQ-15"
source: "https://docs.mergify.com/configuration/conditions"
---

# Deployment attributes

Successful and failed deployments per environment, optionally with an app prefix.

## Sub-features

- `success`
- `failure`

## How to get to it (user POV)

- Conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- App identity only with an app token.
