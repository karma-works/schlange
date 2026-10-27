---
id: queue-pause
title: "Pause the queue"
priority: P2
arztagent: false
status: missing
inventory:
  - "api:PUT /repos/{owner}/{repository}/merge-queue/pause"
  - "api:DELETE /repos/{owner}/{repository}/merge-queue/pause"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pause"
  - "api:PUT /repos/{owner}/{repository}/queue/pause"
  - "api:DELETE /repos/{owner}/{repository}/queue/pause"
  - "api:GET /repos/{owner}/{repository}/queue/pause"
  - "api:POST /repos/{owner}/{repository}/queue/pause/delete"
  - "cli:mergify queue pause"
  - "cli:mergify queue unpause"
problems:
  - "MQ-46"
source: "https://docs.mergify.com/merge-queue/pause"
---

# Pause the queue

The whole queue can be stopped with a reason and started again. Running merges finish atomically
or not at all.

## Sub-features

- `pause`
- `resume`
- `status`

## How to get to it (user POV)

- CLI or API.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Rush after resuming (MQ-46).
