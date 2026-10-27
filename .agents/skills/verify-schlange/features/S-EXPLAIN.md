---
id: S-EXPLAIN
title: "`schlange pr explain`"
priority: P1
arztagent: false
status: missing
inventory: []
problems:
  - "MQ-43"
  - "MQ-45"
  - "W-13"
source: "KONZEPT.md, section 8"
---

# `schlange pr explain`

Explains per pull request why it is (not) queued or merged: per condition the value, its source
and the configuration line.

## Sub-features

- `cause-tree`
- `config-line`
- `json`

## How to get to it (user POV)

- `schlange pr explain <n>`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Planned.** Needs a forge adapter; in the simulation through the event log.

## Gotchas

- Must give a machine-readable reason for every waiting pull request.
