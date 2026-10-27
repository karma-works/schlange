---
id: queue-draft-pr
title: "Queue pull requests as drafts on `mergify/merge-queue/`"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.queue_branch_prefix"
  - "config:queue_rules.allow_inplace_checks"
  - "default:queue_rules.queue_branch_prefix"
  - "default:queue_rules.allow_inplace_checks"
  - "config:queue_rules.batch_pull_request"
  - "default:queue_rules.batch_pull_request"
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

- `prefix`: default `mergify/merge-queue/`, followed by a random hex part.
- `draft-pr`: title and body refer to the contained pull requests.
- `in-place`: computed, not configured: only when `max_parallel_checks` is 1, mode `serial`, batch size 1 (the max of a range), `max_checks_retries` 0 and `merge_conditions` empty or equal to `queue_conditions`. The option `allow_inplace_checks` is deprecated.
- `batch-pull-request`: experimental option `batch_pull_request` (default true), added to the schema on 2026-10-27 without a description; presumably whether the queue opens a batch pull request at all. Behaviour unknown until documented or observed.
- `cleanup`: branch and pull request disappear after merge or abort (I3).

## How to get to it (user POV)

- A draft pull request of the bot appears; in Arztagent `pr-gate.yml` recognises it by branch prefix and bot login.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Check I3 after quiescence: temporary branches = branches in use.

## Gotchas

- `pr-gate.yml` checks the login `mergify[bot]`; Schlange has another login, which needs one more workflow line (KONZEPT section 2).
- Never clean up user branches, only the strict name pattern (MQ-36).
