---
id: conditions-files-other
title: "Further file and line attributes"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:added-files"
  - "attr:modified-files"
  - "attr:removed-files"
  - "attr:added-lines"
  - "attr:modified-lines"
  - "attr:deleted-lines"
problems:
  - "MQ-07"
source: "https://docs.mergify.com/configuration/conditions"
---

# Further file and line attributes

Separate lists of added, modified and removed files or lines.

## Sub-features

- `files`
- `lines`

## How to get to it (user POV)

- Conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Large diffs: limits of the forge API.
