---
id: conditions-dependabot
title: "Dependabot attributes"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:dependabot-update-type"
  - "attr:dependabot-dependency-name"
  - "attr:dependabot-dependency-type"
unsupported:
  - "attr:dependabot-dependency-name"
  - "attr:dependabot-dependency-type"
problems:
  - "MQ-51"
  - "W-23"
source: "https://docs.mergify.com/configuration/conditions"
---

# Dependabot attributes

For Dependabot pull requests the queue reads metadata from the commit messages: kind of update
(patch, minor, major), name and type of the dependency. Arztagent queues patch and minor updates
automatically.

## Sub-features

- `update-type`: built in the evaluator. `version-update:semver-patch|minor|major`, set per pull request in the simulation; reading it from Dependabot's commit trailers comes with the forge adapter.
- `dependency-name`, `dependency-type`: missing (listed as `unsupported`).

## How to get to it (user POV)

- A Dependabot pull request with a patch update is queued without a human.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; `#3` (Dependabot, minor update) is queued without `KI-Review`, `#4` (major update) stays out.

## Gotchas

- The metadata lives in Dependabot's commit trailers; grouped updates have several entries.
