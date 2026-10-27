---
id: direct-merge
title: "Direct merge without queue CI"
priority: P2
arztagent: false
status: missing
inventory: []
problems:
  - "MQ-28"
  - "W-15"
source: "https://docs.mergify.com/merge-queue/direct-merge"
---

# Direct merge without queue CI

A queued pull request that is alone at the front of its lane and already contains the tip of the
base branch is merged at once, without a batch pull request and without another CI run: its own
green CI already describes what lands. With scopes, a pull request that is behind only by queue
merges in scopes it does not touch qualifies as well. Nothing has to be switched on. It does not
apply when the queue checks in place, uses two-step CI, or merges with `merge-batch`, so it is not
in effect for Arztagent.

## Sub-features

- `up-to-date`: the head contains the base tip → merge directly; also the one case where `fast-forward` qualifies.
- `disjoint-scopes`: behind only by recorded queue merges in other scopes → merge directly (not with `fast-forward`).
- `exclusions`: not with in-place checks, two-step CI or `merge-batch`; not for barrier pull requests.

## How to get to it (user POV)

- A pull request that is up to date and green enters an empty queue and is merged within seconds, without a draft pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned scenario: single-step queue, an up-to-date pull request, expected merge without a queue branch; plus the same with two-step CI, expected batch pull request.

## Gotchas

- Must not weaken I0: the landed tree equals the tree the pull request's own CI ran on.
- Arztagent's two-step CI excludes it; a test with Arztagent's config must show a batch pull request.
