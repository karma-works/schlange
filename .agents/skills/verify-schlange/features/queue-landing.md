---
id: queue-landing
title: "Landing as compare-and-swap with evidence"
priority: P1
arztagent: true
status: missing
inventory: []
problems:
  - "MQ-02"
  - "MQ-05"
  - "MQ-06"
  - "MQ-31"
  - "MQ-32"
  - "MQ-35"
source: "https://docs.mergify.com/merge-queue/lifecycle"
---

# Landing as compare-and-swap with evidence

Landing happens only when the base still is exactly the state that was tested and the pull request
head has not changed. Afterwards Schlange checks that the landed tree equals the tested tree. A
journal entry precedes every side effect.

## Sub-features

- `cas`: expected old SHA of the base, never `force`.
- `head-bound`: every transition is bound to the head SHA (I1).
- `tree-check`: tree of the landed commit = tested tree (I0), otherwise halt.
- `journal`: restart after a crash from journal plus forge state.
- `unclear-error`: after a 5xx or timeout, read the state first, never land twice.

## How to get to it (user POV)

- Invisible to users; visible in the evidence (`S-AUDIT`).

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- Deterministic simulation with a crash at every side effect; model checking.

## Gotchas

- No Mergify configuration path, but the core of the drop-in promise.
