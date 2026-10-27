---
id: ci-insights
title: "CI Insights"
priority: P3
arztagent: false
status: missing
inventory:
  - "api:GET /ci/{owner}/queue/stats/{stat}"
  - "api:GET /ci/{owner}/runners/stats/{stat}"
problems:
  - "W-15"
source: "https://docs.mergify.com/ci-insights"
---

# CI Insights

Metrics about CI jobs and runners. A separate product; last for Schlange, or never.

## Sub-features

- `queue-stats`
- `runner-stats`

## How to get to it (user POV)

- Dashboard.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not planned.** No verification path; the feature stays `missing` on purpose.

## Gotchas

- –
