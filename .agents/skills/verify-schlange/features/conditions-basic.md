---
id: conditions-basic
title: "Attributes `base`, `draft`, `label`, `author`, `sender`"
priority: P1
arztagent: true
status: missing
inventory:
  - "attr:base"
  - "attr:draft"
  - "attr:label"
  - "attr:author"
  - "attr:sender"
problems:
  - "MQ-09"
  - "MQ-10"
  - "MQ-11"
source: "https://docs.mergify.com/configuration/conditions"
---

# Attributes `base`, `draft`, `label`, `author`, `sender`

The simple pull request attributes Arztagent uses: target branch, draft state, labels, author and
(in `commands_restrictions`) the sender of a command.

## Sub-features

- `base`: target branch as text.
- `draft`: boolean, negatable (`-draft`).
- `label`: list; label `halt` as stop.
- `author`: login, bots too (`dependabot[bot]`).
- `sender`: login of the commenter for commands.

## How to get to it (user POV)

- Set label `halt` → the pull request leaves the queue.

## Driving it

- `schlange sim run scenarios/p0-queue-conditions.yaml` (base, draft, label, author).

## Gotchas

- Resolve logins by ID rather than name, otherwise renames break (W-06).
