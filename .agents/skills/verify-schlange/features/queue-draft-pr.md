---
id: queue-draft-pr
title: "Queue pull requests as drafts on `mergify/merge-queue/`"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.queue_branch_prefix"
  - "config:queue_rules.allow_inplace_checks"
  - "default:queue_rules.queue_branch_prefix"
  - "default:queue_rules.allow_inplace_checks"
  - "config:queue_rules.batch_pull_request"
  - "default:queue_rules.batch_pull_request"
unsupported:
  - "config:queue_rules.batch_pull_request"
problems:
  - "MQ-18"
  - "MQ-36"
  - "MQ-40"
  - "MQ-41"
  - "W-14"
source: "https://docs.mergify.com/merge-queue/draft-pull-requests"
---

# Queue pull requests as drafts on `mergify/merge-queue/`

The queue tests on a temporary branch with a fixed prefix and a draft pull request on it, which
triggers the normal pull request CI. Checking a pull request in place (on its own branch, without
a draft) is not an option the user sets: it is computed, and happens only when the queue checks
one pull request at a time in serial mode without retries and with single-step CI. Arztagent
(batches of 5, two-step CI) therefore always gets draft pull requests.

## Sub-features

- `prefix`: built. Queue branches are `queue_branch_prefix` (default `mergify/merge-queue/`) plus ten hex characters; a draft pull request is opened on each.
- `draft-pr`: built in the simulated forge (one draft per car); the exact title and body Mergify writes are not reproduced.
- `in-place`: built. Computed, not configured: only when `max_parallel_checks` is 1, mode `serial`, batch size 1 (the max of a range), `max_checks_retries` 0 and `merge_conditions` empty or equal to `queue_conditions`, and the pull request is up to date with its base; then no queue branch and no draft. A pull request behind its base is tested through a draft instead of being updated in place (deviation, see `p1-in-place.yaml`).
- `allow-inplace-checks`: the deprecated option is accepted and ignored, as the documentation says the value is computed; not flagged by `config validate`.
- `batch-pull-request`: missing (listed as `unsupported`). Experimental option `batch_pull_request` (default true), added to the schema on 2026-10-27 without a description; presumably whether the queue opens a batch pull request at all. Only setting it explicitly is flagged: the default presumably matches what Schlange does, and flagging it would flag every configuration with a queue rule.
- `cleanup`: built. Queue branch and draft disappear after landing, failure, dequeue, reset and foreign push; I3 checks it after every step.

## How to get to it (user POV)

- A draft pull request of the bot appears; in Arztagent `pr-gate.yml` recognises it by branch prefix and bot login.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **In place.** Run `target/release/schlange sim run scenarios/p1-in-place.yaml`. Log has `car 2: check #1 in place` (no queue branch for #1) and, for #2 (behind main), a car `on mergify/merge-queue/…`; both land. Exit 0.
- **Drafts for batches.** Run `target/release/schlange sim run scenarios/p1-batch-five.yaml` (Arztagent's configuration: batches of five, two-step CI). Every car runs on a `mergify/merge-queue/…` branch with a draft; `(checks #1 #2 #3)` and `(checks #4 #5 #6 #7 #8)`. Exit 0.
- **Cleanup.** In both runs the invariant report shows `I3 checked`: after every simulated step the queue branches (prefix plus ten hex characters) and open drafts on the forge are exactly those of the engine's cars.

## Gotchas

- `pr-gate.yml` checks the login `mergify[bot]`; Schlange has another login, which needs one more workflow line (KONZEPT section 2).
- Never clean up user branches, only the strict name pattern (MQ-36).
