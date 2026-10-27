---
id: queue-bot-accounts
title: "Bot accounts for merge, draft and update"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:queue_rules.merge_bot_account"
  - "config:queue_rules.draft_bot_account"
  - "config:queue_rules.update_bot_account"
problems:
  - "W-23"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Bot accounts for merge, draft and update

Instead of acting as the app, the queue can merge, open drafts or push updates in the name of a
user account.

## Sub-features

- `merge`
- `draft`
- `update`

## How to get to it (user POV)

- Option in the queue rule.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Needs a user token; act as the app by default.
