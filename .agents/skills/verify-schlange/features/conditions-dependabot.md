---
id: conditions-dependabot
title: "Dependabot attributes"
priority: P1
arztagent: true
status: missing
inventory:
  - "attr:dependabot-update-type"
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

- `update-type`: `version-update:semver-patch|minor|major`.
- `dependency-name`, `dependency-type`: lists from the metadata.

## How to get to it (user POV)

- A Dependabot pull request with a patch update is queued without a human.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- The metadata lives in Dependabot's commit trailers; grouped updates have several entries.
