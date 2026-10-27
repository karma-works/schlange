---
id: S-DOCTOR
title: "`schlange doctor`"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-13"
  - "MQ-29"
  - "W-22"
source: "KONZEPT.md, section 8"
---

# `schlange doctor`

Checks version, configuration, app permissions, ruleset exception, webhook delivery and rate
limit; serves as the container's health check.

## Sub-features

- `version`: built.
- `schema`: built (reports whether a schema is found).
- `matrix`: built (support matrix embedded).
- `app-permissions`, `ruleset`, `webhooks`, `rate-limit`: missing.

## How to get to it (user POV)

- `schlange doctor`

## Driving it

- `schlange doctor --json`; exit 0 without FAIL.

## Gotchas

- The container has no schema; that is a WARN, not a FAIL.
