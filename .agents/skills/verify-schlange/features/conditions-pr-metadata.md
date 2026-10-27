---
id: conditions-pr-metadata
title: "Further pull request metadata"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:merged"
  - "attr:closed"
  - "attr:locked"
  - "attr:linear-history"
  - "attr:number"
  - "attr:merged-by"
  - "attr:merge-commit-sha"
  - "attr:milestone"
  - "attr:head"
  - "attr:head-repo-full-name"
  - "attr:title"
  - "attr:body"
  - "attr:body-raw"
  - "attr:repository-name"
  - "attr:repository-full-name"
  - "attr:assignee"
  - "attr:commits-unverified"
  - "attr:co-authors"
  - "attr:commits-behind"
  - "attr:commits"
  - "attr:updates"
  - "attr:updated-at"
  - "attr:created-at"
  - "attr:closed-at"
  - "attr:merged-at"
problems:
  - "MQ-09"
source: "https://docs.mergify.com/configuration/conditions"
---

# Further pull request metadata

State, texts, commits, timestamps and origin of a pull request as conditions.

## Sub-features

- `state`
- `texts`
- `commits`
- `timestamps`

## How to get to it (user POV)

- Conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- `commits-behind` only with the length operator.
