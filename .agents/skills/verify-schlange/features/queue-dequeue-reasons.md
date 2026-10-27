---
id: queue-dequeue-reasons
title: "Dequeue with a reason"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:queue-dequeue-reason"
  - "enum:queue-dequeue-reason:*"
unsupported:
  - "attr:queue-dequeue-reason"
problems:
  - "MQ-45"
  - "MQ-09"
  - "MQ-11"
  - "MQ-07"
  - "MQ-43"
  - "W-08"
  - "W-13"
source: "https://docs.mergify.com/merge-queue/lifecycle"
---

# Dequeue with a reason

When a pull request leaves the queue, a reason from a fixed list of codes always comes with it,
for example because the queue conditions no longer hold, a check failed, a conflict appeared or a
human dequeued it. The code can also be read as a condition attribute.

## Sub-features

- `automatic`: built. Every dequeue carries a code and a text (`crates/schlange-core/src/reasons.rs`); a pull request whose checks end while it stays queued gets a `checks ended` line with a code too.
- `codes`: built for these paths; codes marked *assumed* are our choice where the documentation does not name the path:

  | Path | Code | Leaves the queue |
  |---|---|---|
  | merge conditions false, checks done | `CHECKS_FAILED` | yes |
  | only cancelled runs | `CHECKS_INTERRUPTED` | yes |
  | fixed `checks_timeout` passed | `CHECKS_TIMEOUT` | yes |
  | conflict with the base | `CONFLICT_WITH_BASE_BRANCH` | yes |
  | conflict with a pull request ahead | `CONFLICT_WITH_PULL_AHEAD` | no (held) |
  | pushed while queued | `PULL_REQUEST_UPDATED` | yes |
  | merged by hand | `PR_MANUALLY_MERGED` | yes |
  | base branch of the pull request changed | `BASE_BRANCH_CHANGED` | yes |
  | queue rule gone | `QUEUE_RULE_MISSING` | yes |
  | foreign push to its queue branch | `DRAFT_PULL_REQUEST_CHANGED` | yes |
  | split limit reached | `BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS` | yes |
  | dropped untested in the last round | `DROPPED_BY_BISECTION_ELIMINATION` | yes |
  | squash refused at landing | `PR_UNEXPECTEDLY_FAILED_TO_MERGE` | yes |
  | queue_conditions false (label, draft, red check) | `PR_DEQUEUED` *assumed* | yes |
  | closed | `PR_DEQUEUED` *assumed* | yes |
  | car ahead failed | `BATCH_AHEAD_FAILED` | no |
  | pull request ahead left | `PR_AHEAD_DEQUEUED` | no |
  | another pull request of the same batch left | `BATCH_REBUILT` | no |
  | car ahead edited by someone else | `BATCH_AHEAD_BRANCH_EDITED` | no |
  | base moved outside the queue | `MERGE_QUEUE_RESET` *assumed* | no |
  | sent back after a split | `BATCH_AHEAD_FAILED` *assumed* | no |

- `no-requeue-loop`: built. After `CHECKS_FAILED`, `CHECKS_TIMEOUT`, `CHECKS_INTERRUPTED`, conflicts and split drops, the `queue` action does not queue the same head again; a new push does.
- `own-rebase`: not applicable yet: Schlange never pushes to a pull request (in-place updates are not built), so its own updates cannot dequeue (W-08).
- `attribute`: missing (listed as `unsupported`: the evaluator has no `queue-dequeue-reason` attribute).
- `manual`: missing (`PR_MANUALLY_DEQUEUED` needs the `dequeue` command).

## How to get to it (user POV)

- Status comment and label `dequeued` on the pull request; `schlange pr explain` names the reason.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Every path in a scenario.** Each `scenarios/p1-*.yaml` asserts the code of its dequeues with `expect: { pr: N, state: dequeued, reason: CODE }`; run e.g. `target/release/schlange sim run scenarios/p1-push-to-queued-pr.yaml` → `dequeue #1 from `default`: PULL_REQUEST_UPDATED: the head moved from … to …`. Assumed codes print `(code assumed)` in the log.
- **Golden event logs.** `cargo test -p schlange-forge --test scenarios` compares each scenario's log with `crates/schlange-forge/tests/golden/<name>.log`.
- **Closed, draft, manual merge.** `cargo test -p schlange-forge --test dsl`.

## Gotchas

- Many codes concern features outside phase 1 (scopes, partitions); they stay in this file's inventory so that no code gets lost.
- The schema's enum is upper case (`CHECKS_FAILED`), the documentation writes kebab case (`checks-failed`); Schlange reports the schema form.
- `PR_DEQUEUED` for "queue conditions no longer hold" and for "closed" is a guess; a difference test against Mergify must settle it. Mergify's `BATCH_PULL_REQUEST_CLOSED` (someone closes the draft) is not handled.
- Draft: a pull request converted to draft leaves only because Arztagent's queue_conditions contain `-draft`; with conditions without `-draft` it would stay (MQ-09 says Mergify dequeues; not verified).
