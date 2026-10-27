---
id: queue-bot-accounts
title: "Bot accounts for merge, draft and update"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.merge_bot_account"
  - "config:queue_rules.draft_bot_account"
  - "config:queue_rules.update_bot_account"
  - "default:queue_rules.draft_bot_account"
  - "default:queue_rules.merge_bot_account"
  - "default:queue_rules.update_bot_account"
problems:
  - "MQ-51"
  - "W-23"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Bot accounts for merge, draft and update

Without bot accounts (the default, in effect for Arztagent) the app itself creates the draft
pull requests, merges and updates. Optionally the queue acts in the name of a user account for
each of these.

## Sub-features

- `default-app`: no account set → the app creates drafts, merges and updates (P1).
- `merge`: merge as a user account (P2).
- `draft`: open drafts as a user account (P2).
- `update`: update pull requests as a user account (P2).

## How to get to it (user POV)

- Option in the queue rule.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Needs a user token; act as the app by default.
