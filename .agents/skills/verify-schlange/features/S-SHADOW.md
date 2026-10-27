---
id: S-SHADOW
title: "Shadow mode"
priority: P1
arztagent: false
status: missing
inventory: []
problems:
  - "MQ-45"
  - "W-01"
  - "W-02"
source: "KONZEPT.md, section 8"
---

# Shadow mode

Schlange decides as in real operation but does not act, and logs every decision for comparison
with Mergify.

## Sub-features

- `decide-without-acting`
- `compare`

## How to get to it (user POV)

- Operating mode of the server.

## Driving it

- Difference test against Mergify on Arztagent.

## Gotchas

- Two weeks without a differing decision are the acceptance of phase 1.
