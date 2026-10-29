---
id: queue-dequeue-reasons
title: "Dequeue with a reason"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:queue-dequeue-reason"
  - "enum:queue-dequeue-reason:*"
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
- `codes`: built for these paths. A code is *not* assumed when the docs name it for the path (`checks-failed`, `checks-timeout`, `checks-interrupted`, `batch-max-failure-resolution-attempts`, `dropped-by-bisection-elimination`) or when the code's own name states exactly that path; it is *assumed* where we picked the closest code for a path its name does not state (critic 1abc: the flags were inverted before):

  | Path | Code | Leaves the queue |
  |---|---|---|
  | merge conditions false, checks done | `CHECKS_FAILED` | yes |
  | only cancelled runs | `CHECKS_INTERRUPTED` | yes |
  | fixed `checks_timeout` passed | `CHECKS_TIMEOUT` | yes |
  | conflict with the base | `CONFLICT_WITH_BASE_BRANCH` | yes |
  | `reset_on_external_merge: never`: the squash no longer applies on the moved base | `CONFLICT_WITH_BASE_BRANCH` | yes |
  | conflict with a pull request ahead | `CONFLICT_WITH_PULL_AHEAD` | no (held) |
  | pushed while queued | `PULL_REQUEST_UPDATED` | yes |
  | merged by hand | `PR_MANUALLY_MERGED` | yes |
  | base branch of the pull request changed | `BASE_BRANCH_CHANGED` | yes |
  | queue rule gone | `QUEUE_RULE_MISSING` | yes |
  | foreign push to its queue branch | `DRAFT_PULL_REQUEST_CHANGED` | yes |
  | split limit reached | `BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS` | yes |
  | dropped untested in the last round | `DROPPED_BY_BISECTION_ELIMINATION` | yes |
  | squash refused at landing (tested on exactly this base) | `PR_UNEXPECTEDLY_FAILED_TO_MERGE` | yes |
  | landing refused, the car's own pull requests that did not land | the refusal's reason (`MERGE_QUEUE_RESET` *assumed* when the base moved, `BATCH_REBUILT` when one of them left) | no |
  | someone closed the car's draft pull request | `BATCH_PULL_REQUEST_CLOSED` (staying queued is our choice) | no |
  | the car behind a closed draft | `BATCH_AHEAD_REBUILT` *assumed* | no |
  | queue_conditions false (label, draft, red check) | `PR_DEQUEUED` *assumed* | yes |
  | closed | `PR_DEQUEUED` *assumed* | yes |
  | car ahead failed | `BATCH_AHEAD_FAILED` | no |
  | pull request ahead left (also a refused landing ahead and an edited branch ahead) | `PR_AHEAD_DEQUEUED` | no |
  | another pull request of the same batch left | `BATCH_REBUILT` | no |
  | the batch ahead was rebuilt without a pull request | `BATCH_AHEAD_REBUILT` | no |
  | its own attributes fail the merge conditions | `PR_DEQUEUED` *assumed* | until the attributes change |
  | checks failed, retried on a fresh draft | `CHECKS_RETRIED` | no |
  | car ahead edited and the edit accepted (`allow_queue_branch_edit` with `merge-batch`/`fast-forward`) | `BATCH_AHEAD_BRANCH_EDITED` | no |
  | base moved outside the queue | `MERGE_QUEUE_RESET` | no |
  | the queue halted (tree check) | `MERGE_QUEUE_RESET` *assumed* | no |
  | sent back after a split | `BATCH_AHEAD_FAILED` *assumed* | no |
  | `dequeue` command | `PR_MANUALLY_DEQUEUED` | yes |
  | `queue <other queue>` command while queued | `PR_MANUALLY_DEQUEUED` *assumed* | yes, then waits for the other queue |
  | a built-in protection holds a queued pull request (body edited) | `PR_DEQUEUED` *assumed* | yes |

- `sticky-dequeue`: built. A pull request dequeued because its queue conditions stopped holding (`PR_DEQUEUED`: label `halt`, back to draft) is not queued again at the same head when the cause goes away; it waits with the hold `dequeued-waits-for-new-head` (reason in `explain` and in `holds`). A new head lifts it. Observed on Mergify with Arztagent's configuration in the sandbox (s05, s07: the pull request stayed open for 40 minutes after the cause went away). **Deliberate deviation:** a `queue` command from a sender that `commands_restrictions` allows also lifts it; the old S11 reference recorded an open PR after a wait for closure, but its timeout provenance is unavailable, so it does not establish Mergify's outcome or its cause. S18 is prepared for a fresh bounded observation with confirmed queue-entry and dequeue checkpoints. An agent removing `halt` never restarts a merge. Tests: `scenarios/p1-sticky-dequeue.yaml`, `p1-label-halt.yaml`, `cargo test -p schlange-github-fake --test e2e_arztagent a_dequeued_pull_request_waits_for_a_new_head_or_a_listed_human`.
- `no-requeue-loop`: built. After `CHECKS_FAILED`, `CHECKS_TIMEOUT`, `CHECKS_INTERRUPTED`, conflicts and split drops, the `queue` action does not queue the same head again; a new push does, and so does a `queue` command (which resets the status, docs: commands/queue).
- `own-rebase`: not applicable yet: Schlange never pushes to a pull request (in-place updates are not built), so its own updates cannot dequeue (W-08).
- `attribute`: built. `queue-dequeue-reason` is the code of the last time the pull request's queue checks ended (a dequeue or a `checks ended` line), `NONE` before; `=` and `!=` accept the documented kebab case (`checks-failed`) as well as the schema's upper case.
- `manual`: built. `@mergifyio dequeue` → `PR_MANUALLY_DEQUEUED`; the pull request does not rejoin on its own.

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
- `PR_DEQUEUED` for "queue conditions no longer hold" and for "closed" is a guess; a difference test against Mergify must settle it. Whether Mergify keeps the pull requests of a closed draft queued (`BATCH_PULL_REQUEST_CLOSED`) is not documented; Schlange keeps them.
- Tests for the critic's paths: `cargo test -p schlange-core --test dequeue_codes`.
- Draft: a pull request converted to draft leaves only because Arztagent's queue_conditions contain `-draft`; with conditions without `-draft` it would stay (MQ-09 says Mergify dequeues; not verified).
