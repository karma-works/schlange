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
- `rest-client`: built. Blocking HTTPS client (ureq on rustls with the ring provider, roots from `SSL_CERT_FILE` or the system bundle, no OpenSSL); ETags with `If-None-Match`; pagination over `Link` (`client.rs`, `api.rs`). Calls: pull requests, files, commits, check runs, check suites, combined status, refs and matching refs, git commits, compare, contents, collaborator permission, branch protection, rules for a branch, rulesets; create/delete ref, read-compare-`PATCH force:false` on a ref, `POST /merges`, draft pull requests, close, squash merge with `sha`, check runs, comments, reactions, labels.
- `webhooks`: built. `POST /webhook`, HMAC-SHA256 (`X-Hub-Signature-256`, constant-time compare), delivery ids deduplicated (last 10 000), payloads of `pull_request` (opened, synchronize, reopened, closed, edited, labeled, unlabeled, ready_for_review, converted_to_draft), `check_run`, `check_suite`, `status`, `issue_comment`, `push`, `branch_protection_rule`, `repository_ruleset` parsed into core events and read hints (`webhook.rs`, `service.rs`). Commands in comments: missing.
- `reconcile`: built. At start and every `--reconcile-every` seconds (default 300): every open pull request, the engine's pull requests and branches, base branches, every queue branch; followed by an orphan sweep (queue branches and drafts that belong to no car). Progress without webhooks.
- `queue-cars`: built. Queue branch `<prefix><10 hex>` at the expected base, pull request heads merged with the merges API, a draft pull request on it; a merge conflict deletes the branch and is reported to the engine (`forge.rs`).
- `landing`: built for `squash`. Base read and compared right before each `PUT /pulls/{n}/merge` with the tested head as `sha`; afterwards the landed commit's parent and tree are compared with the expected base and the tested tree; a different tree stops the landing and the engine halts the queue. GitHub offers no compare-and-swap on the base (see `api.rs`). Other merge methods: missing.
- `unknown-outcomes`: built (MQ-35). Writes that fail with 5xx or a timeout are not repeated blindly: the adapter reads (pull request merged? branch or draft created? merge commit present?) and only then repeats; a decision that cannot be carried out is kept and retried with backoff.
- `provisional-reads`: built (W-24). Reads are retried with jitter on 5xx and 404; a pull request that cannot be read is left out of the engine's snapshot; a branch counts as deleted only after a canary read of the default branch works.
- `check-runs`: built. The queue's own check run (`Schlange merge queue`) on each queued pull request: in progress, success when merged, neutral with the dequeue code when it leaves. Check runs of Schlange's own app are ignored when conditions are evaluated.
- `rate-limits`: built. One token bucket per process: landing always goes, reads wait, cosmetic writes (check run, comments, labels) are deferred below a reserve; `Retry-After`, `x-ratelimit-remaining: 0` with `x-ratelimit-reset`, and secondary limits block the whole client.
- `permissions`: built (W-22). `metadata: read`, `contents: write`, `pull_requests: write`, `checks: write`, `statuses: read`; `workflows: write` only when pull requests that change workflows go through the queue (`permissions.rs`). `schlange doctor` compares.
- `ruleset-exception`: checked by `schlange doctor`; setting it up by script: missing.
- `journal`: missing. A restart recovers from GitHub by reconciliation (orphans are swept, the queue is rebuilt); there is no journal and no SQLite yet.

## How to get to it (user POV)

- Create a GitHub App with the permissions above, install it, then:
  `SCHLANGE_GITHUB_APP_ID=… SCHLANGE_GITHUB_PRIVATE_KEY=key.pem SCHLANGE_WEBHOOK_SECRET=… SCHLANGE_REPOS=owner/name schlange serve`
  (flags: `--app-id`, `--private-key`, `--webhook-secret[-file]`, `--repo`, `--installation-id`, `--reconcile-every`, `--listen`, `--config`, `--api-url`).
- The app's webhook URL is `http(s)://<host>/webhook`; `GET /healthz`, `GET /status` (JSON).
- `schlange doctor` with the same environment checks the app.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.
- No real GitHub: everything below runs against the fake GitHub (`crates/schlange-github-fake`), an HTTP server backed by the simulated git that sends signed webhooks to `schlange serve`.

- **End to end.** Run `cargo test -p schlange-github-fake --test e2e`. Eleven tests pass: Arztagent's `.mergify.yml` read from the repository, a draft on `mergify/merge-queue/<10 hex>`, merge conditions on the draft, squash landing; a batch of three; a red batch bisected; all webhooks lost (reconciliation only); duplicated and reordered webhooks; a lying 5xx on the merge (MQ-35); lying 5xx on draft creation and merges; 404/5xx storms; rate limits; a restart with orphan sweep; push and `halt` label while queued. Each ends with the fake's invariant check (I0 on the base, no double landing, I3).
- **Client and endpoint.** Run `cargo test -p schlange-github-fake --test client`. Token cache, ETag 304, pagination, retries, ambiguous writes, rate limits, cosmetics deferred, every queue call, signatures and duplicate deliveries, doctor checks, and the invariant checker catching an untested landing.
- **Binary.** Run `cargo test -p schlange --test serve`. The real binary `schlange serve`, configured by environment, lands a pull request on the fake; `schlange doctor --json` reports `github-app`, `webhooks`, `rate-limit` OK.

## Gotchas

- The bot login is `<app slug>[bot]`, not `mergify[bot]`; Arztagent's `pr-gate.yml` needs one more line.
- Only a real sandbox can confirm: the merges API with a SHA as `head`, `PUT /pulls/{n}/merge` answering 405 vs 409, whether `bypass_actors` is visible with `metadata: read`, whether `branch_protection_rule`/`repository_ruleset` webhooks arrive without `administration: read`, draft pull requests on private repositories without a paid plan, and which workflows the queue's pushes and drafts trigger (MQ-18).
- The fake's semantics are ours: file-level merges (no line-level merge), immediate consistency unless faults are injected.
