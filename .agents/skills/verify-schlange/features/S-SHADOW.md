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
  sandbox): drafts by `--mergify-login` on the queue prefix list their pull requests as `#n`;
  pull requests merged by that login landed through Mergify (`merged_by`, `merged_at`);
  Mergify's check run (app `--mergify-app`) on a pull request means queued; a dequeue code in
  a concluded run's title or summary is the reason it left.
- `compare`: built. Per pull request: queued, landed, landing order (rank among the pull
  requests both landed; Mergify's merges in one second keep their batch's order), the batch
  it landed in, the dequeue code. Verdicts `agree`, `disagree`, `inconclusive` (Mergify merged
  before Schlange decided), `pending`. Stored in SQLite (`shadow` table), shown by
  `schlange shadow report [--json]` (exit 1 on a disagreement) and
  `GET /repos/{owner}/{repo}/shadow/report` behind the admin token.
- `read-only-app`: built. A read-only app (metadata, contents, issues, pull requests,
  checks, statuses: `read`, `permissions::SHADOW_PERMISSIONS`) is enough: `serve --shadow` asks for
  read-only installation tokens, `schlange doctor --shadow` accepts the app and warns about any
  write permission, and skips the ruleset exception. Tests: `cargo test -p schlange-github-fake
  --test shadow a_read_only_app_is_enough_for_shadow_mode` (the fake GitHub refuses endpoints
  without the permission GitHub asks for), `--test client
  doctor_accepts_a_read_only_app_for_shadow_mode_only`. Setup guide: `docs/shadow-mode.md`.
- `acceptance-run`: missing. Two weeks on Arztagent next to the real Mergify (KONZEPT.md,
  section 11) have not happened; only the fake GitHub's scripted other queue was compared.
  W-01/W-02 latency is not measured against Mergify yet.

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

## Gotchas

- The scripted other queue in the fake is ours; Mergify's real check-run names and draft
  descriptions must be confirmed on the sandbox before the comparison means anything.
- Schlange cannot test its own drafts in shadow; a batch Mergify never built stays pending.
- Two weeks without a differing decision are the acceptance of phase 1.
