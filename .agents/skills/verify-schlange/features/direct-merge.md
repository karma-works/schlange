---
id: direct-merge
title: "Direct merge without queue CI"
priority: P2
arztagent: false
status: partial
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

- `up-to-date`: built (`crates/schlange-core/src/queue.rs`, `fill`). A pull request alone at the front (no car, batch of one) whose head contains the base tip and whose own CI already meets the merge conditions is checked where it is and lands, without a queue branch, a draft or a queue CI run (log: `car N: merge #n directly`). `fast-forward` is not built, so that variant is not either.
- `disjoint-scopes`: missing (scopes are not built); such a pull request takes the normal path with a draft.
- `exclusions`: built. Not when the queue checks in place, with two-step CI, or with `merge-batch`; barrier pull requests need scopes (not built).

## How to get to it (user POV)

- A pull request that is up to date and green enters an empty queue and is merged within seconds, without a draft pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-direct-merge.yaml`: #1 is up to date with green CI and merges directly (`car 2: merge #1 directly`); #2 is behind main after a direct push and goes through a draft (`+ #2 (checks #2)`). Arztagent's two-step CI keeps drafts in every Arztagent scenario (`p1-arztagent-two-step`).

## Gotchas

- Must not weaken I0: the landed tree equals the tree the pull request's own CI ran on.
- Arztagent's two-step CI excludes it; a test with Arztagent's config must show a batch pull request.
