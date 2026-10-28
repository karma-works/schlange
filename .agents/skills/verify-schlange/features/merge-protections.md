---
id: merge-protections
title: "Merge protections and auto-merge"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:merge_protections"
  - "config:merge_protections.*"
  - "config:merge_protections_settings"
  - "config:merge_protections_settings.*"
  - "default:merge_protections_settings.*"
  - "enum:reporting_method:*"
  - "default:merge_protections.*"
unsupported:
  - "config:merge_protections_settings*"
  - "default:merge_protections_settings.*"
  - "enum:reporting_method:*"
problems:
  - "MQ-13"
source: "https://docs.mergify.com/merge-protections"
---

# Merge protections and auto-merge

Custom rules that show up as a check on the pull request and block merging until they hold; plus
auto-merge without a queue.

## Sub-features

- `rule`: built for the merge queue. A `merge_protections` entry whose `if` holds on the pull request must have its `success_conditions` hold too, or the queue does not land it: its car waits green at the front and lands once the conditions hold (re-evaluated on every event; every minute when a condition reads the clock, such as `schedule`). Our reading of the docs: with the merge queue the protections gate the merge, not the entry into the queue. I1 checks every landing against them (`crates/schlange-forge/src/invariants.rs`).
- `report`: partial. The hold shows in the status comment ("It may not merge yet: merge protection `…` is not satisfied") and in the status API (`held`); the `Mergify Merge Protections` check run or deployment is not reported (`reporting_method`, `post_comment`: missing).
- `auto-merge`: missing (`merge_protections_settings.auto_merge_conditions`).

## How to get to it (user POV)

- Section `merge_protections`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- Run `target/release/schlange sim run scenarios/p2-merge-protections.yaml`: #1 changes `db/` without `db-approved`; its checks pass, the status comment says it may not merge yet, it lands at +0:30 when the label is added, #2 after it. Exit 0. Also on real git (`localgit` test).

## Gotchas

- Reporting as check run or deployment: not built; a merge made outside the queue is not stopped by Schlange.
