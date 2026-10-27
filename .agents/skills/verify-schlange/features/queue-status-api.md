---
id: queue-status-api
title: "Queue status through API and CLI"
priority: P2
arztagent: false
status: missing
inventory:
  - "api:GET /repos/{owner}/{repository}/merge-queue/status"
  - "api:GET /repos/{owner}/{repository}/merge-queue/branches"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pull/{pr_number}"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pull/{pr_number}/dequeue-impact"
  - "api:POST /repos/{owner}/{repository}/merge-queue/pull/{pr_number}/dequeue"
  - "api:GET /repos/{owner}/{repository}/queues"
  - "api:GET /repos/{owner}/{repository}/queues/branches"
  - "api:GET /repos/{owner}/{repository}/queues/configuration"
  - "cli:mergify queue status"
  - "cli:mergify queue show"
problems:
  - "MQ-43"
  - "W-13"
  - "W-25"
source: "https://docs.mergify.com/api-usage"
---

# Queue status through API and CLI

Read the state of the queue, of single pull requests and the effect of a dequeue; dequeue a pull
request through the API.

## Sub-features

- `status`
- `pull-request`
- `dequeue-impact`
- `dequeue`
- `configuration`

## How to get to it (user POV)

- CLI `schlange queue status` (planned), MCP.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Schlange output is `--json` with a stable schema (S-CLI).
