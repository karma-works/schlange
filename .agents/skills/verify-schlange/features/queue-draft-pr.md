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
problems:
  - "MQ-18"
  - "MQ-36"
  - "MQ-40"
  - "MQ-41"
  - "W-14"
source: "https://docs.mergify.com/merge-queue/draft-pull-requests"
---

# Queue pull requests as drafts on `mergify/merge-queue/`

For a batch or an outdated pull request the queue creates a temporary branch with a fixed prefix
and a draft pull request on it. That pull request triggers the normal pull request CI. A single
pull request that is already up to date may be tested in place.

## Sub-features

- `prefix`: default `mergify/merge-queue/`, followed by a random hex part.
- `draft-pr`: title and body refer to the contained pull requests.
- `in-place`: on by default; an up-to-date single pull request is tested without a queue pull request.
- `cleanup`: branch and pull request disappear after merge or abort (I3).

## How to get to it (user POV)

- A draft pull request of the bot appears; in Arztagent `pr-gate.yml` recognises it by branch prefix and bot login.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- Check I3 after quiescence: temporary branches = branches in use.

## Gotchas

- `pr-gate.yml` checks the login `mergify[bot]`; Schlange has another login, which needs one more workflow line (KONZEPT section 2).
- Never clean up user branches, only the strict name pattern (MQ-36).
