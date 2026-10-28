# Critique of phase 1a (commit a55d238)

Critique subagent (Claude, fresh context, own worktree), 2026-10-27. Verdict: **deviating**.
Reproductions: `scenarios/regressions/` once fixed (see the fix round).

## Top reasons
1. **Blocker:** bisection can get stuck. A test car chosen to land turns red on the fresh read and
   stays `Failed`, with nothing handling it (`queue.rs` handles `Failed` only without a test;
   `bisect::resolve` returns early when `land` is set). This violates I2. With delayed
   webhooks the seed sweep hits it 138 times in 1600 runs, and an innocent PR is dequeued
   while the culprit stays.
2. **Major (MQ-42):** a merge check that never reports on the draft fails the car with
   `CHECKS_FAILED` and an empty list as soon as any other check finished. The expected result is
   a timeout naming the missing check. With `auto`, the car waits forever.
3. **Major:** the harness cannot see races, because forge replies are synchronous and never
   fail. There is no crash/restart, no HTTP or webhook layer, no 5xx or lying forge, no delayed
   reads, git is file-level only, CI timing is uniform, and the generators cover too little.

## Other findings
- **Dynamic batch size** `{min,max}` divides by all slots, not by free slots, and deviates from
  the documented example (10 PRs, 5 checks → five batches of two).
- **Attribute-only `merge_conditions`** (for example a label) turn bisection test cars red and
  blame the wrong pull request.
- **Ignored options:** `reset_on_external_merge: never`, `allow_queue_branch_edit: true`,
  `max_checks_retries > 0` and `checks_timeout: auto` are silently ignored and not flagged by
  `config validate --strict`, because `partial` counts as supported at feature granularity.
- **Lost `Refresh` reply:** `Verifying` and `Confirming` wait for a matching token and
  reconciliation snapshots carry none. One lost reply leaves the car stuck for good, and
  `base_requested` is one-shot.
- **Rebuilt cars:** cars behind a rebuilt car get `PR_AHEAD_DEQUEUED`; the enum also has
  `BATCH_AHEAD_REBUILT`.
- **Stale feature files:** `queue-draft-pr` says missing and "not drivable" although
  `p1-in-place` exists. The support matrix reports `attr:author`, `check-success` and
  `pull_request_rules` as missing although the simulation uses them.
- **I0** is weaker than KONZEPT.md for squash batches (intermediate commits are checked against
  untested prefix trees). That matches Mergify's semantics, but KONZEPT was not amended.
  "Green" can be vacuous without check conditions.
- **I1** compares no tested PR heads (ancestor check only). It is near-tautological with
  synchronous reads.
- **I4** does not count CI that keeps running on closed drafts.
- **I5** does not compare dequeue reasons, and its opt-outs are broad.
- **MQ coverage** is overstated for MQ-02, 06, 11, 21, 22, 26, 37 and 42.
- **Clean room:** OK.

## Conformant
Fixed batch size, wait timer, split examples, cascade, the two-step view, conflicts (base,
ahead, external `always`), the fixed timeout, CAS landing (in the simulation), the in-place
rule, and the dequeue codes, all of which exist in the schema enum.
