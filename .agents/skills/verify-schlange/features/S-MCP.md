---
id: S-MCP
title: "MCP server"
priority: P2
arztagent: false
status: missing
inventory: []
problems:
  - "W-25"
source: "KONZEPT.md, section 8"
---

# MCP server

MCP over stdio and HTTP with the tools queue status, explain, validate, simulate, events, enqueue
and dequeue.

## Sub-features

- `stdio`
- `http`
- `permissions`

## How to get to it (user POV)

- MCP client

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Planned.** Contract tests of the tools.

## Gotchas

- Enqueue and dequeue with the same permissions as `commands_restrictions`.
