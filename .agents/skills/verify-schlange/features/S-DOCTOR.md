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

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Binary.** Run `target/release/schlange doctor`. Exit `0`; lines `OK version`, `OK support-matrix`, `OK schema` (or `WARN schema` without `.research/`), `SKIP` for the GitHub checks.
- **Container.** Run `docker run --rm --read-only --cap-drop ALL schlange:verify doctor`. Exit `0`; `WARN schema` because the image holds no Mergify file.
- **Proof.** `schlange doctor --json` gives `ok: true` and one entry per check.

## Gotchas

- The container has no schema; that is a WARN, not a FAIL.
