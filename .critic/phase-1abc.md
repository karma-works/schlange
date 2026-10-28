# Critique at 8593b32 (phase 1a fixes, 1b localgit, 1c GitHub adapter)

Critique subagent (Claude, fresh context, own worktree), 2026-10-27. Verdict: **deviating**.
The simulation core holds up well; the GitHub adapter does not yet.

## Previous findings (`.critic/phase-1a.md`)
Fixed:
- stuck bisection (regression passes on sim and localgit; the innocence oracle at 1000 cases);
- MQ-42;
- the harness race gaps (fault layer, crash journal, localgit, fake GitHub);
- dynamic batch size;
- attribute blame;
- the lost-reply wedge.

Partly fixed: the stale feature files, and the MQ coverage claims.

## GitHub adapter
- **G1 (blocker):** `CloseCar` drops a pending `BuildCar`, but not a pending `Land`
  (`crates/schlange-github/src/forge.rs` around 544). A retried landing can merge a pull
  request that was already dequeued, for example one labelled `halt` while the merge PUT kept
  failing with 502. The retry also skips the tree check, so I1 is violated. The fake GitHub
  has no I1 check, so its e2e tests passed.
- **G2 (major):** landing is not a compare-and-swap. The merge PUT guards only the PR head,
  not the base. A base push during a rate-limit wait lands on the foreign commit. The tree
  check catches this after the fact; it does not prevent it. `api.rs` has `update_ref_cas`,
  but landing does not use it.
- **G3:** one transient 405 dequeues the pull request for good
  (`PR_UNEXPECTEDLY_FAILED_TO_MERGE`).
- **G4:** a permanent 4xx is retried forever (for example a 403 from `/merges` without
  `workflows` permission), so the queue stalls.
- **G5:** an empty webhook secret is accepted.
- **G6:** a single-threaded listener without a body-read timeout lets one slow client stall
  webhooks and health.
- **Other:**
  - `/status` is unauthenticated on the public listener and leaks private PR data; its
    `outcomes` grow without bound.
  - The delivery id is recorded even on 400/503, so GitHub's redelivery is dropped.
  - `http://` API URLs are accepted without a warning.
  - `Link` next URLs are followed to any host, with the token attached.
  - `combined_status` is not paginated.
  - `mergeable: null` is treated as "no conflict".
  - No journal or persistence in `serve` (`State::default()` at start), so `auto` history is
    lost on every restart.
  - `serve` never runs the validator.
  - The installation token is not downscoped.

## Core and conditions
- **B1:** a retry after a timeout reuses the same branch name while the old close is
  unconfirmed, and breaks I4 under forge faults. Reproduction: two PRs, one hanging,
  `max_checks_retries: 1`, `max_parallel_checks: 1`, `forge_faults: standard`.
- **Dequeue codes:**
  - A landing refusal of the queue's own PRs dequeues with no code at all.
  - Cars behind a refusal get `BATCH_REBUILT`; the documented code is `PR_AHEAD_DEQUEUED`.
  - Cars behind a foreign push get `BATCH_AHEAD_BRANCH_EDITED`, although that code is only
    for `allow_queue_branch_edit`.
  - With `never` plus a conflicting external push, the code is
    `PR_UNEXPECTEDLY_FAILED_TO_MERGE` instead of `CONFLICT_WITH_BASE_BRANCH` (repro R1).
  - The "assumed" flags are inverted.
  - `BATCH_PULL_REQUEST_CLOSED` is never emitted.
- **Retries:** bisection split tests are never retried, but the docs show retried splits (R3).
- **Batches:** batches are formed in queue order, not grouped by similarity as the docs
  describe (R4).
- **Direct merge:** missing and not flagged (R5).
- **Branch edits:** dequeue only `car.own`, not "every PR embarked".
- **conditions-files:** renames ignore `previous_filename`, so `git mv` bypasses Arztagent's
  constitution gate (security).
- **Globs:** `**/*.py` misses root files; `{a,b}` and `[seq]` are treated as literals.
- **conditions-checks:**
  - Newest run is taken per name only, not per app and workflow/event.
  - `@app/name` is missing.
  - Status `seq` is mixed with check-run ids.
- **conditions-syntax:**
  - `≥` and `≤` are rejected.
  - `not:` over an evaluation error fails open.
  - A regex runtime error counts as no match.
  - `{{ author }}` is missing.
- **Rules:** `pull_request_rules` without `conditions` match every PR in `serve`.
- **`--strict`:** exits 2 for every configuration, even `{}`, and misses value-level gaps.
- **YAML 1.1:** `yes` and `on` are rejected.
- **S-CLI:** clap usage errors exit 2 (collides with "unsupported") and are not JSON.
- **Doctor:** does not load `.mergify.yml`, and counts `partial` as supported.

## Invariants
- I0 in `never` mode accepts any parent mismatch: it does not check that the commits between
  the tested base and the parent are external.
- For check-named conditions, "green" is not tied to `draft.head_sha == car.head`.
- A halt hides wedges from I2.
- The property tests never enable `never`, retries, `auto` or branch edits, and do not cover
  the fake GitHub.

## Harness against bors
**Equivalent:**
- a fake GitHub over HTTP with a state model on in-memory git;
- signed webhooks into the real router;
- a tester DSL;
- a manual clock.

**Beyond bors:**
- property tests with an innocence oracle;
- crash injection;
- reply faults and base races;
- localgit with a real CAS;
- I0–I5 checked every step.

**Missing:**
- snapshot assertions for the adapter;
- a database in tests;
- command tests;
- I1 in the fake;
- fake fidelity: null mergeability, transient 405, rulesets, workflow-permission refusal,
  read lag, status pagination.

## Clean room
OK.
