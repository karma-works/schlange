---
id: S-AUDIT
title: "Evidence per merge"
priority: P1
arztagent: false
status: missing
inventory: []
problems:
  - "MQ-02"
  - "MQ-31"
source: "KONZEPT.md, section 8"
---

# Evidence per merge

For every merge an evidence record: tested SHA and tree, base, pull request heads, landed SHA;
available through CLI and MCP.

## Sub-features

- `record`
- `query`

## How to get to it (user POV)

- `schlange audit <pr>` (planned).

## Driving it

- In the simulation from the event log.

## Gotchas

- –
