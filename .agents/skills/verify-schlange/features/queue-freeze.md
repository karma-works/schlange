---
id: queue-freeze
title: "Scheduled freeze"
priority: P2
arztagent: false
status: partial
inventory:
  - "api:*/scheduled_freeze*"
  - "cli:mergify freeze *"
unsupported:
  - "api:PATCH /repos/{owner}/{repository}/scheduled_freeze/{freeze_id}"
  - "cli:mergify freeze *"
problems:
  - "MQ-46"
source: "https://docs.mergify.com/merge-protections/freeze"
---

# Scheduled freeze

For a time window, pull requests matching given conditions are not merged.

## Sub-features

- `hold`: built. A freeze (`engine::Freeze`: reason, start, optional end, matching conditions, exclude conditions) holds back the landing of every queued pull request it matches while it is active; checks still run, the car waits green and lands when the freeze ends (the engine wakes at the end). Exclude conditions are combined with AND (all must match to let a pull request through). An unreadable condition never lets anything through. In serial mode a car behind a frozen one waits too (it was tested on top of it). I1 checks every landing against the freezes in force.
- `create`: built. `POST /repos/{owner}/{repo}/scheduled_freeze` in `schlange serve` (admin token): `reason`, `start` (default now), `end` (optional), `timezone` (only UTC; other zones are refused, use an offset in the timestamp), `matching_conditions`, `exclude_conditions` (each a condition string). A freeze that ends in the past is refused.
- `delete`: built. `DELETE …/scheduled_freeze/{id}` and `POST …/{id}/delete`.
- `list`: built. `GET …/scheduled_freeze`, also in the queue status API (`freezes`).
- `update`: missing (`PATCH`; delete and create instead).
- `cli`: missing (`mergify freeze …` has no `schlange` counterpart yet).
- `report`: partial. The hold shows in the status comment ("It may not merge yet: frozen by …") and in the status API (`is_frozen`, `held`); the `Mergify Merge Protections` check is not reported.

## How to get to it (user POV)

- The queue API of `schlange serve`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Simulation.** Run `target/release/schlange sim run scenarios/p2-freeze.yaml`: #2 (`hotfix` and `urgent`) lands at +0:11, #1 waits green until the freeze ends at +0:40, #3 (only `hotfix`) after it. Exit 0.
- **serve against the fake GitHub.** `cargo test -p schlange-github-fake --test e2e_p2 a_scheduled_freeze_holds_matching_pull_requests_until_it_ends`.

## Gotchas

- Time zones of the windows: only UTC (and explicit offsets) are read; anything else is refused rather than guessed.
- Freezes are queue controls (engine state, journaled), not part of the configuration; a freeze applies to every base branch unless its conditions say otherwise.
