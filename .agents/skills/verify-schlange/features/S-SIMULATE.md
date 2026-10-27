---
id: S-SIMULATE
title: "`schlange config simulate`"
priority: P2
arztagent: false
status: missing
inventory: []
problems:
  - "W-12"
source: "KONZEPT.md, section 8"
---

# `schlange config simulate`

Evaluates a new configuration against real pull requests without acting.

## Sub-features

- `against-pr`

## How to get to it (user POV)

- `schlange config simulate --pr <n>`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Planned.** Needs a forge adapter.

## Gotchas

- –
