---
id: github-integration
title: "GitHub integration (app, webhooks, reconciliation)"
priority: P1
arztagent: true
status: partial
inventory: []
problems:
  - "MQ-18"
  - "MQ-29"
  - "MQ-30"
  - "MQ-33"
  - "MQ-34"
  - "MQ-35"
  - "W-01"
  - "W-22"
  - "W-24"
source: "https://docs.mergify.com/integrations/github"
---

# GitHub integration (app, webhooks, reconciliation)

Schlange works as a GitHub App: webhooks are hints, a periodic reconciliation is the truth. Queue
pull requests, check runs and merges go through the API. The app needs a ruleset exception so that
batch merges do not fail on the queue's own rules.

## Sub-features

- `app-auth`: built. App JWT (RS256 with ring), installation tokens cached until five minutes before expiry, a 401 renews the token (`crates/schlange-github/src/jwt.rs`, `client.rs`).
- `rest-client`: built. Blocking HTTPS client (ureq on rustls with the ring provider, roots from `SSL_CERT_FILE` or the system bundle, no OpenSSL); ETags with `If-None-Match`; pagination over `Link`, followed only on the API host (`client.rs`, `api.rs`). Requests never leave the API host (absolute URLs and redirects elsewhere are refused, so the token stays there); plain `http://` is refused unless `--insecure-test-api` is given (fake GitHub only); a renamed repository's 301 (reads) and 307 (writes) are followed on the same host. A rate-limited **write** is never repeated after the wait: it comes back to the caller, which re-validates from fresh reads. Installation tokens are downscoped to the served repositories and to Schlange's permissions as far as the installation grants them (W-22). Combined statuses are read across all pages. Calls: pull requests, files, commits, check runs, check suites, combined status, refs and matching refs, git commits, compare, contents, collaborator permission, branch protection, rules for a branch, rulesets; create/delete ref, read-compare-`PATCH force:false` on a ref, `POST /merges`, draft pull requests, close, squash merge with `sha`, check runs, comments, reactions, labels.
- `listener`: built. Own small HTTP server (`http.rs`): a pool of handler threads behind a bounded queue (503 when full), a socket read timeout and an overall deadline per request, header and body limits, `Content-Length` required. A slow client holds one handler for at most the deadline; webhooks and `/healthz` keep working (`http::tests::a_slow_client_does_not_stall_the_others`). `/status` needs `Authorization: Bearer <admin token>` (`--admin-token-file` or `SCHLANGE_ADMIN_TOKEN`) and is off without one; it shows at most 200 outcomes.
- `webhooks`: built. `POST /webhook`, HMAC-SHA256 (`X-Hub-Signature-256`, constant-time compare), a secret of at least 16 bytes (`serve` refuses a shorter or empty one; on the command line it draws a warning), delivery ids recorded only after a delivery was accepted (a 400/401/503 is handled when GitHub redelivers it) and kept in SQLite across restarts, payloads of `pull_request` (opened, synchronize, reopened, closed, edited, labeled, unlabeled, ready_for_review, converted_to_draft), `check_run`, `check_suite`, `status`, `issue_comment`, `push`, `branch_protection_rule`, `repository_ruleset` parsed into core events and read hints (`webhook.rs`, `service.rs`). Commands in comments: missing.
- `reconcile`: built. At start and every `--reconcile-every` seconds (default 300): every open pull request, the engine's pull requests and branches, base branches, every queue branch; followed by an orphan sweep (queue branches and drafts that belong to no car). Progress without webhooks.
- `queue-cars`: built. Queue branch `<prefix><10 hex>` at the expected base, pull request heads merged with the merges API, a draft pull request on it; a merge conflict deletes the branch and is reported to the engine (`forge.rs`).
- `landing`: built for `squash`, **partial as a compare-and-swap**. Each `PUT /pulls/{n}/merge` (tested head as `sha`) is preceded by fresh reads of the pull request (open, at the tested head) and of the base (exactly the expected commit). Afterwards the landed commit's parent and tree are compared with the expected base and the tree the car tested for that prefix (kept in SQLite across restarts); a different tree stops the landing and the engine halts the queue. The merge API has no expected-base parameter, so a push between the base read and GitHub executing the call is detected afterwards, not prevented. The ref-update path (Git Data API commit plus `PATCH /git/refs` with `force: false`) would be a true compare-and-swap but leaves pull requests "closed", not "merged", and needs a bypass that skips every rule; the trade-off is in `api.rs`. Other merge methods: missing.
- `landing-retries`: built (G1). A failed landing is never retried by the adapter: after a backoff the engine gets `Failed`, reads the pull requests and the base, re-checks its conditions and decides anew. `CloseCar` drops everything pending for its car. A pull request labelled `halt` while the merge call kept failing is dequeued and never merged (`landing a_halt_label_during_failing_merge_calls_never_lands`); a base push during a rate-limit wait does not land on the foreign commit (`landing a_base_push_during_a_rate_limit_wait_does_not_land_on_the_foreign_commit`).
- `refusals`: built (G3/G4). A transient 405 or 409 on the merge call is retried three times, each after re-reading the pull request and the base; a permanent refusal (403 without `workflows: write`, 422, a 405 that stays) dequeues with `PR_UNEXPECTEDLY_FAILED_TO_MERGE`, GitHub's message and a hint. A queue branch merge GitHub refuses for good (403/422) replies `CarRefused`: the pull request leaves the queue, the others get a new car. `mergeable: null` is read again and never taken for "no conflict".
- `unknown-outcomes`: built (MQ-35). Writes that fail with 5xx or a timeout are not repeated blindly: the adapter reads (pull request merged? branch or draft created? merge commit present?) and only then decides; a build, close or read that cannot be carried out is kept and retried with backoff.
- `read-your-writes`: built. A pull request the adapter merged stays merged, a draft it closed stays closed, and a branch read that shows a commit the adapter moved the branch away from is read again (and reported as unavailable while it stays stale).
- `provisional-reads`: built (W-24). Reads are retried with jitter on 5xx and 404; a pull request that cannot be read is left out of the engine's snapshot; a branch counts as deleted only after a canary read of the default branch works.
- `check-runs`: built. The queue's own check run (`Schlange merge queue`) on each queued pull request: in progress, success when merged, neutral with the dequeue code when it leaves. Check runs of Schlange's own app are ignored when conditions are evaluated.
- `rate-limits`: built. One token bucket per process: landing always goes, reads wait, cosmetic writes (check run, comments, labels) are deferred below a reserve; `Retry-After`, `x-ratelimit-remaining: 0` with `x-ratelimit-reset`, and secondary limits block the whole client.
- `permissions`: built (W-22). `metadata: read`, `contents: write`, `pull_requests: write`, `checks: write`, `statuses: read`; `workflows: write` only when pull requests that change workflows go through the queue (`permissions.rs`). `schlange doctor` compares.
- `ruleset-exception`: checked by `schlange doctor`; setting it up by script: missing.
- `journal`: built. SQLite in WAL mode with `synchronous = FULL` in `--data-dir` (default `/data`): the engine state is checkpointed after every step before its side effects, every side effect is journaled as an intent before it is sent. A restart continues from the checkpoint, feeds `Restarted` and reconciles: a landing in doubt is resolved from GitHub's state, nothing lands twice, the queue (order, `since`) is kept (`serve a_crash_mid_landing_recovers_the_queue_and_never_lands_twice`). Also stored: `checks_timeout: auto` history, handled command comments (by comment id and version, part of the engine state; a redelivered command is not run again after a restart: `serve command_dedup_survives_a_restart`), webhook delivery ids, car prefix trees, an audit row per landing (S-AUDIT). A journal that cannot be written stops the worker before its next side effect; `/healthz` then answers 503.
- `config`: built. One path in `serve` and the doctor (`crates/schlange-github/src/config.rs`): the configuration file is read from the default branch only, searched under the six documented names in their order (two files are an error and nothing is applied), reloaded on a push to the default branch and at every reconciliation, and validated like `config validate` (stages 1–4; stage 5 findings are logged as warnings); an invalid configuration is not applied, the last valid one stays in force, a change feeds `ConfigurationChanged` (every pull request is evaluated again), the error shows in the log, in `/status` and in `schlange doctor`. A rule without `conditions` is rejected (schema and `Rules`). `serve` does not start without the schema (`--schema` or `SCHLANGE_SCHEMA`).

## How to get to it (user POV)

- Create a GitHub App with the permissions above, install it, then:
  `SCHLANGE_GITHUB_APP_ID=… SCHLANGE_GITHUB_PRIVATE_KEY=key.pem SCHLANGE_WEBHOOK_SECRET=… SCHLANGE_REPOS=owner/name schlange serve`
  (flags: `--app-id`, `--private-key`, `--webhook-secret-file` (or the environment), `--repo`, `--installation-id`, `--reconcile-every`, `--listen`, `--config`, `--api-url`, `--data-dir`, `--schema`, `--admin-token-file`, `--insecure-test-api` for tests only).
- The app's webhook URL is `https://<host>/webhook` (behind a TLS proxy); `GET /healthz`, `GET /status` (JSON, admin token).
- `schlange doctor` with the same environment checks the app.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.
- No real GitHub: everything below runs against the fake GitHub (`crates/schlange-github-fake`), an HTTP server backed by the simulated git that sends signed webhooks to `schlange serve`.

- **End to end.** Run `cargo test -p schlange-github-fake --test e2e`. Eleven tests pass: Arztagent's `.mergify.yml` read from the repository, a draft on `mergify/merge-queue/<10 hex>`, merge conditions on the draft, squash landing; a batch of three; a red batch bisected; all webhooks lost (reconciliation only); duplicated and reordered webhooks; a lying 5xx on the merge (MQ-35); lying 5xx on draft creation and merges; 404/5xx storms; rate limits; a restart with orphan sweep; push and `halt` label while queued. Each ends with the fake's invariant check (I0 on the base, no double landing, I3).
- **Landing under faults.** Run `cargo test -p schlange-github-fake --test landing`: the critic's G1 reproduction, a base push during a rate-limit wait, transient 405/409, a permanent 422, a workflow change without the permission, `mergeable: null`, more than 30 commit statuses, read-after-write lag.
- **Service.** Run `cargo test -p schlange-github-fake --test serve`: a crash right after a landing and the restart from the journal, configuration validation with the last valid configuration kept, delivery ids recorded only on success and kept across restarts, downscoped tokens.
- **Random.** Run `cargo test -p schlange-github-fake --test properties` (`SCHLANGE_GH_PBT_CASES=50` for a long run): random pull requests, CI, labels, pushes, base pushes, API and webhook faults, null mergeability and read lag; I0, I1, I3 and no double landing after the queue drains.
- **Golden decision logs.** Run `cargo test -p schlange-github-fake --test golden_decisions` (`UPDATE_GOLDEN=1` rewrites `tests/golden/*.txt`).
- **Client and endpoint.** Run `cargo test -p schlange-github-fake --test client`. Token cache, ETag 304, pagination, retries, ambiguous writes, rate limits (a rate-limited write is handed back, not repeated), cosmetics deferred, renames, other hosts refused, every queue call, signatures and duplicate deliveries, doctor checks, and the invariant checker catching an untested landing.
- **Binary.** Run `cargo test -p schlange --test serve`. The real binary `schlange serve`, configured by environment, lands a pull request on the fake; `schlange doctor --json` reports `github-app`, `webhooks`, `rate-limit` OK.

## Gotchas

- The bot login is `<app slug>[bot]`, not `mergify[bot]`; Arztagent's `pr-gate.yml` needs one more line.
- Only a real sandbox can confirm: the merges API with a SHA as `head`; the exact status codes and messages of `PUT /pulls/{n}/merge` (405 vs 409 vs 422, the wording of "base branch was modified" and of the workflow refusal); how long `mergeable` stays `null` and how far reads lag behind writes; whether redirects after a rename are 301/307 as assumed; whether downscoped token requests are accepted with exactly these permission names; whether `bypass_actors` is visible with `metadata: read`; whether `branch_protection_rule`/`repository_ruleset` webhooks arrive without `administration: read`; draft pull requests on private repositories without a paid plan; which workflows the queue's pushes and drafts trigger (MQ-18); and how often a push lands inside the one-request window between the base read and the merge call.
- The fake's semantics are ours: file-level merges (no line-level merge), immediate consistency unless faults (`set_read_lag`, `set_mergeable_lag`) are injected. A squash whose tree, parent and message equal the pull request's single commit gets the same SHA in the fake (content-addressed), unlike GitHub.
