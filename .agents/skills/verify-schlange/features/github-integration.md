---
id: github-integration
title: "GitHub integration (app, webhooks, reconciliation)"
priority: P1
arztagent: true
status: missing
inventory: []
problems:
  - "MQ-29"
  - "MQ-30"
  - "MQ-33"
  - "MQ-34"
  - "MQ-35"
  - "W-01"
  - "W-22"
  - "W-24"
source: "https://docs.mergify.com/integrations/github"
---

# GitHub integration (app, webhooks, reconciliation)

Schlange works as a GitHub App: webhooks are hints, a periodic reconciliation is the truth. Queue
pull requests, check runs and merges go through the API. The app needs a ruleset exception so that
batch merges do not fail on the queue's own rules.

## Sub-features

- `webhooks`: idempotent, delivery ID deduplicated, HMAC verified.
- `reconcile`: at start and periodically; progress also without webhooks.
- `check-runs`: the queue's own check.
- `ruleset-exception`: by script, like `mergify-setup.sh` in Arztagent.
- `rate-limits`: central token bucket, landing before cosmetics.

## How to get to it (user POV)

- Install the app, then `schlange doctor`.

## Driving it

- Adapter `github` is missing; first `sim` → `localgit` → `forgejo`.

## Gotchas

- The bot login is not `mergify[bot]`; Arztagent's `pr-gate.yml` needs one more line.
