---
id: queue-pause
title: "Pause the queue"
priority: P2
arztagent: false
status: partial
inventory:
  - "api:PUT /repos/{owner}/{repository}/merge-queue/pause"
  - "api:DELETE /repos/{owner}/{repository}/merge-queue/pause"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pause"
  - "api:PUT /repos/{owner}/{repository}/queue/pause"
  - "api:DELETE /repos/{owner}/{repository}/queue/pause"
  - "api:GET /repos/{owner}/{repository}/queue/pause"
  - "api:POST /repos/{owner}/{repository}/queue/pause/delete"
  - "cli:mergify queue pause"
  - "cli:mergify queue unpause"
problems:
  - "MQ-46"
source: "https://docs.mergify.com/merge-queue/pause"
---

# Pause the queue

The whole queue can be stopped with a reason and started again. Running merges finish atomically
or not at all.

## Sub-features

- `pause`: built. `Event::Control { Pause }` (`crates/schlange-core/src/gates.rs`). Pull requests can still join; nothing new is checked and nothing lands; running queue checks are cancelled (their drafts closed, the pull requests stay queued with `PR_CHECKS_STOPPED_BECAUSE_MERGE_QUEUE_PAUSE`). A landing already sent finishes. A check in place is not cancelled (as documented) and does not land while paused; if it fails, the pull request leaves.
- `resume`: built. `Unpause`; the queue fills its slots again (bounded by `max_parallel_checks`, MQ-46).
- `status`: built. The pause is in the engine state, journaled in serve's SQLite store (it survives a restart), shown in the queue status API (`pause`), in `schlange queue status` and in the status comment of every queued pull request.
- `api`: built in `schlange serve` behind the admin token: `PUT|DELETE|GET /repos/{owner}/{repo}/merge-queue/pause` and the `queue/pause` spellings, `POST queue/pause/delete`. `PUT` needs a `reason` (at most 255 characters).
- `cli`: built as `schlange queue pause --reason …` and `schlange queue unpause` (the Mergify CLI is not used).
- `dashboard`: missing.

## How to get to it (user POV)

- `schlange queue pause --reason "CI incident" --url … --repo owner/name` with `SCHLANGE_ADMIN_TOKEN`, or the API.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Simulation.** Run `target/release/schlange sim run scenarios/p2-pause.yaml`. At +0:05 `the queue is paused: CI incident`, both cars are closed with `PR_CHECKS_STOPPED_BECAUSE_MERGE_QUEUE_PAUSE`, #3 is enqueued while paused, nothing is built until `the queue is unpaused` at +0:30, then all three land. I1 checks that no landing happened while paused. Exit 0.
- **serve against the fake GitHub.** `cargo test -p schlange-github-fake --test e2e_p2 the_queue_api_pauses_and_resumes_and_shows_the_status` (API with and without the token, the pause in the journaled state).
- **CLI.** `cargo test -p schlange --test serve` runs `schlange queue pause|status|unpause` against a running `schlange serve`.

## Gotchas

- Rush after resuming (MQ-46): bounded by `max_parallel_checks`; not measured.
- The pause applies to every base branch of the repository (the docs pause "the merge queue").
