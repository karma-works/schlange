---
id: queue-bot-accounts
title: "Bot accounts for merge, draft and update"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.merge_bot_account"
  - "config:queue_rules.draft_bot_account"
  - "config:queue_rules.update_bot_account"
  - "default:queue_rules.draft_bot_account"
  - "default:queue_rules.merge_bot_account"
  - "default:queue_rules.update_bot_account"
unsupported:
  - "config:queue_rules.merge_bot_account"
  - "config:queue_rules.draft_bot_account"
  - "config:queue_rules.update_bot_account"
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

- `default-app`: built. Without accounts the app itself acts: GitHub calls use the installation
  token only, so drafts are opened, queue branches written and pull requests squash-merged by
  `<slug>[bot]` (`e2e_commands arztagent_constitution_pull_request_waits_for_a_listed_human`
  checks the draft's author and the merger; the simulated forge writes as `schlange[bot]`).
  Schlange never updates pull requests yet. `Merged-by` trailers name the app when no command
  queued the pull request.
- `merge`, `draft`, `update`: missing (listed as unsupported): acting as a user account needs a
  user token, which Schlange does not take.

## How to get to it (user POV)

- Option in the queue rule.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Default.** `cargo test -p schlange-github-fake --test e2e_commands arztagent_constitution_pull_request_waits_for_a_listed_human` (draft author and merger are `schlange[bot]`).
- **Unsupported options.** `target/release/schlange config validate --strict` exits 2 for a configuration that sets an account.

## Gotchas

- Needs a user token; act as the app by default.
