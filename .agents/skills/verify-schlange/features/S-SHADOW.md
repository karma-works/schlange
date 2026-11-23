---
id: S-SHADOW
title: "Shadow mode"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-45"
  - "W-01"
  - "W-02"
source: "KONZEPT.md, section 8"
---

# Shadow mode

`schlange serve --shadow` runs next to Mergify on the same repository: it reads everything,
decides as in real operation, writes nothing to GitHub, records what it would have done and
compares that with what Mergify did, per pull request.

## Sub-features

- `decide-without-acting`: built (`crates/schlange-github/src/shadow.rs`). The worker hands no
  write decision to the GitHub adapter; the shadow layer answers them. A car exists only as a
  virtual branch and draft (numbers from 1 000 000 000); its checks are **borrowed** from
  Mergify's draft that embarks the same pull requests in the same order on the same base
  (otherwise they stay pending, and the batch already differs). `Land` gets no answer: after
  the reply timeout the engine reads GitHub and counts a Mergify merge as its own landing
  (the lost-reply path, MQ-35). No sweep, no cosmetics, no journal; reads only.
- `never-writes`: built twice. Besides the worker, a write gate on the client
  (`crate::lease::ShadowGate`) refuses every request but a `GET` (installation tokens
  excepted) before it is sent, and panics in tests.
- `observe-mergify`: built, as our reading of what GitHub shows (to be confirmed on the
  sandbox): drafts by `--mergify-login` on `ShadowConfig::mergify_queue_prefix` list their
  pull requests in the title's terminal `(#n, #m)` list or a dedicated embarkment section;
  the title list takes precedence over body sections about PRs ahead. Incidental references in member titles
  and other body sections are ignored. A sandbox-observed fallback requires the nonempty,
  ordered, duplicate-free title `#N` list (excluding the draft itself) to equal both the `#N`
  labels and `/pull/N` targets on the first reference-bearing body line;
  pull requests merged by that login landed through Mergify (`merged_by`, `merged_at`);
  only confirmed draft membership means queued. Generic `Summary` and rule-evaluation runs
  are never queue evidence. A known literal dequeue code in a concluded run's title or
  summary is recorded once per GitHub check-run ID, including for closed PRs whose heads
  come from the recent-pulls read. Literal `PR_CLOSED` is preserved as observer evidence
  even though the engine currently calls this path assumed `PR_DEQUEUED`; this produces
  an honest disagreement pending sandbox evidence. A signed pull-request webhook records a
  recognized queue draft before reconciliation, so an opened and quickly closed draft is not
  lost; replay is deduplicated by draft PR number. The JSON report's `draft_observer` section
  lists only candidate PR numbers at the three structural gates: matching queue prefix,
  matching configured author and parsed membership. It retains no title, body or author
  identity, and its probe rows never become parity rows. Candidate structures retain only
  numeric `#N` and `/pull/N` references, body line indexes/counts, and booleans for list/link
  starts and HTML-comment delimiters, and are retained only for candidates that remain
  unparsed. Missing codes are not inferred from prose.
- `compare`: built. Per pull request: queue-entry count, landed, landing order (rank among the pull
  requests both landed; Mergify's merges in one second keep their batch's order), the batch
  it landed in, and the ordered dequeue-code history. The legacy queued boolean and latest
  dequeue code remain in JSON for report compatibility. Verdicts `agree`, `disagree`,
  `inconclusive` (Mergify merged before Schlange decided), `pending`. Stored in SQLite
  (`shadow` and append-only `shadow_events` tables), shown by
  `schlange shadow report [--json]` (exit 0 only for complete agreement; exit 1 for a
  disagreement, pending/inconclusive row or incomplete repository history) and
  `GET /repos/{owner}/{repo}/shadow/report` behind the admin token.
- `read-only-app`: built. A read-only app (metadata, contents, issues, pull requests,
  checks, actions, statuses: `read`, `permissions::SHADOW_PERMISSIONS`) is enough: `serve --shadow` asks for
  read-only installation tokens, `schlange doctor --shadow` accepts the app and warns about any
  write permission, and skips the ruleset exception. Tests: `cargo test -p schlange-github-fake
  --test shadow a_read_only_app_is_enough_for_shadow_mode` (the fake GitHub refuses endpoints
  without the permission GitHub asks for), `--test client
  doctor_accepts_a_read_only_app_for_shadow_mode_only`. Setup guide: `docs/shadow-mode.md`.
- `history`: built. Queue-entry, batch and dequeue occurrences are append-only in the
  `shadow_events` table, keyed by their durable source identity (draft PR, check-run or Schlange
  car/transition), and survive restarts without replay duplication. The shadow layer also
  records once per pull request, in the database, that it
  saw it open (`seen`). A pull request Mergify merged or closed while shadow mode was not
  watching (before its start, or while it was stopped) is `inconclusive` instead of a
  disagreement (`before_watching` in the JSON); one with nothing but `seen` has no row. The
  text report gives only their number; `schlange shadow report --all` lists them
  (`shadow::tests::the_text_report_hides_what_closed_before_shadow_mode_watched`). Found on the first real run next to
  Arztagent: 20 pull requests merged the day before showed up as disagreements. Tests:
  `--test shadow pull_requests_merged_before_shadow_mode_saw_them_are_inconclusive`,
  `shadow::tests::report_agreements_and_each_kind_of_difference`.
  A watched pull request with no queue events is an explicit zero-entry/zero-dequeue agreement,
  rather than being omitted. Each per-PR occurrence stream is limited to 1,024 rows; reaching
  the limit or failing a durable append sets `history_incomplete` and makes the report
  inconclusive instead of comparing a prefix. A repository is additionally limited to 200,000
  occurrence rows. Startup detects either pre-existing overflow before loading the event rows,
  persists a repository-wide marker and exposes the top-level `history_incomplete` flag. Only
  the literal dequeue code and check-run ID are retained, not the upstream check title or summary.
  If neither an occurrence nor its incomplete-history marker can be stored, the worker leaves
  the durable webhook inbox row pending; restart replays it after storage recovers.
- `acceptance-run`: missing. Two weeks on Arztagent next to the real Mergify (KONZEPT.md,
  section 11) have not happened. Real `schlange-sandbox/queue-lab` runs 45109750499 and
  45113336435 isolated the queue-draft observer format without retaining Mergify prose;
  run 45114951923 then confirmed matching queue membership and entry count for S01 (`1/1`).
  That run remained `inconclusive` for landing: the Actions runner has no webhook ingress and
  the service recorded 17 reconciliations in total, but its decisive read observed Mergify's
  merge before Schlange had recorded a pre-merge landing decision. The report therefore does
  not prove I1 landing parity and correctly refuses to reconstruct a pre-merge read. This is
  evidence for queue observation, not an acceptance result. The fake GitHub supplies the signed-webhook path
  and the active sandbox supplies landing evidence; a real webhook-connected shadow deployment
  and the two-week Arztagent run remain required. W-01/W-02 latency is not measured against
  Mergify yet.

## How to get to it (user POV)

- `schlange serve --shadow [--mergify-login mergify[bot]] [--mergify-app mergify]`
- `schlange doctor --shadow` (read-only app, no ruleset exception)
- `schlange shadow report [--json] --repo owner/name [--url …] [--data-dir …]`
- `GET /repos/{owner}/{repo}/shadow/report` (admin token)

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Fake GitHub, no writes.** `cargo test -p schlange-github-fake --test shadow the_e2e_flows_in_shadow_send_no_write_request`: the e2e flows (draft, batch of three, red batch, halt label, commands, push, pause) in shadow while the fake's scripted other queue does the work; the fake's request log has no request but reads and token requests, the gate saw no attempt, and the report agrees on every pull request both landed.
- **Fake GitHub, report.** `cargo test -p schlange-github-fake --test shadow the_report_agrees_with_the_other_queue_and_names_each_difference`: two pull requests agree (queued, landed, order 1 and 2, batch `[a, b]`); a dequeue code differs (`CHECKS_FAILED` vs `CHECKS_TIMEOUT`) and a pull request only the other queue took are disagreements.
- **Restart.** `cargo test -p schlange-github-fake --test shadow the_report_survives_a_restart_from_the_database`.
- **Short-lived draft.** `cargo test -p schlange-github-fake --test shadow a_short_lived_queue_draft_is_captured_from_its_durable_webhook` closes the queue draft before reconciliation and verifies that replay preserves one entry.
- **Observer storage failure.** `cargo test -p schlange-github-fake --test shadow shadow_storage_failure_leaves_webhook_pending_until_restart_recovers_it` removes both shadow tables, verifies that the delivery stays pending and the live report fails closed, then restarts and observes durable replay.

## Gotchas

- The scripted other queue in the fake is ours. The strict embarkment parser currently
  accepts a terminal parenthesized PR-number list in the title (including the issue's
  concrete `Fix #412 (#500)` example), otherwise its explicit
  `Embarked pull requests:` body section with one leading `#n` or
  `#n: title` entry per line; it stops at an empty line or another section. The sandbox fallback
  accepts only the matching three-view structure described above. Other formats produce no
  membership facts, rather than scraping arbitrary references.
- Check-only queue entry before draft creation remains unobserved until an unambiguous
  real queue-check format is verified. Dequeue outputs containing no known literal code
  likewise remain unobserved; no production equivalence is claimed for these cases.
- Regression tests cover generic summary/rule runs, incidental issue references, nonqueue
  bot branches, closed queued PRs and repeated reconciliation of completed check-run IDs.
  These tests are fake-GitHub evidence, not a replacement for real sandbox rehearsal.
- Persisted facts from an older observer are retained, including potentially false queue
  or batch observations. Archive that database and use a fresh shadow data directory for
  acceptance after this fix; historical disagreements are not silently rewritten.
- Schlange cannot test its own drafts in shadow; a batch Mergify never built stays pending.
- A prior dequeue followed by a new unmatched queue entry is `pending`; matching historical
  prefixes never turn an unfinished latest cycle into agreement.
- Two weeks without a differing decision are the acceptance of phase 1.
