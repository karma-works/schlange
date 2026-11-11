# Arztagent parity and GitHub's last-request window

Decision: 2026-11-01. The acceptance target is a replacement for Mergify under Arztagent's
existing configuration, with comparable operating assumptions and safeguards. It does not
require a stronger atomicity guarantee than GitHub exposes. This qualifies the GitHub adapter's
acceptance contract; it does not relax the core, simulated-forge or localgit invariant checks.

## Journeys regex compatibility

The Arztagent source snapshot retains `-check-failure ~= ^(?!verify-arztagent journeys$)`.
Regex matching considers both the bare check name and `@<app>/<name>` (observed in PR #24).
A failing GitHub Actions Journeys run therefore blocks entry or dequeues an existing pull
request: its qualified name still matches that negative lookahead. Preserving this behavior is
parity with the observed matcher, although it differs from the comment's informational intent.

The test-only derivative `scenarios/configs/arztagent-journeys-informational.yml` uses
`^(?!(?:@github-actions/)?verify-arztagent journeys$)` to exempt both forms. Its fake-GitHub test
checks that this regex is the only configuration difference from the unmodified source
snapshot; another test explicitly checks the actual source's blocking behavior. The simulation
also identifies CI as GitHub Actions instead of using unattributed runs. Any production change
to Arztagent's configuration belongs in the human-authored constitution pull request.

## Base changes before a merge

The GitHub pull-request merge API can bind a merge to an expected PR head, but has no expected
base parameter. A fresh base read and a subsequent merge request are separate operations.
Mergify documents resetting its queue after external base changes and an exclusive mode that
restricts merges to its app. These are operational safeguards, not public evidence of an atomic
expected-base check in its internal implementation.

For Arztagent, accept Schlange when:

- Normal writes to `main` go through one active queue writer, enforced by the effective
  rulesets and classic branch protection. Pause the other queue before enabling this writer.
  Emergency admin writes are an explicit exception and require pausing/reconciling the queue.
- The adapter binds each merge to the tested head and re-reads the PR and expected base before
  each attempt, including after rate limits or backoff. An observed change prevents that attempt
  and causes a fresh engine decision and, where required, rebuilding/retesting.
- The landed parent and tree are verified; a mismatching tree halts the queue and requires
  operator investigation. This detects an untested landing after the fact; it cannot undo it.
- Tests retain the external-base-race cases and expose the residual window. A permitted writer
  racing the final request remains a documented GitHub limitation, not by itself an Arztagent
  parity blocker. The core's stronger model invariant remains checked.

## A stop condition changed just before merging

A label update and a PR merge cannot be one conditional GitHub transaction. There is no public
evidence that Mergify gives a label such as `halt` an atomic cancellation guarantee either.

Schlange must stop a merge when its fresh read observes `halt`, a draft conversion or another
failed gate. A rejected or delayed merge attempt must return to the engine for fresh reads;
the adapter must not blindly retry it. A label set after the last verification read cannot
reliably cancel a merge already being executed by GitHub. This residual window is accepted for
Arztagent parity, not hidden by changing an invariant test. A label is not an emergency abort
of an in-flight GitHub request.

## Webhook ingress and failover

The last-request limitations above do not excuse leaving a webhook on an inactive worker.
New work for a repository is accepted only by a ready single worker, shadow observer or leader.
An uninitialised, stopped, stale or known follower worker returns 503 before storing or deduping
the delivery. A refused delivery id remains usable on the leader or after takeover.

- Use one active instance for the initial Arztagent deployment. With multiple instances,
  route to the leader and retry a refused POST with the exact signed body and delivery id.
  GitHub does **not** automatically redeliver a failed webhook; ingress or an operator must do
  that. Reconciliation cannot recover the editor of a checkbox comment.
- `GET /readyz` is 200 only when every served repository can accept new webhooks; otherwise
  it is 503. `GET /healthz` remains a liveness signal and can be green on a follower. For
  multiple repositories with different leaders use repository-aware routing from authenticated
  `/status`, or separate service instances; the process-wide readiness probe is conservative.
- Admission is a local observation, not a distributed lock: demotion can race acceptance.
  Already persisted rows remain durable. A leader polls its inbox once per second between
  worker operations; start and takeover also replay pending rows. This includes older-version
  deliveries and writes through another connection to the same SQLite data directory.
- Separate hosts with separate volumes do not share an inbox. An accepted delivery on a host
  that subsequently loses leadership waits for that host to recover leadership/restart as
  leader, or for explicit redelivery to the active leader. This is not cross-host replication.
  Do not load-balance blindly across independent instances. Long worker operations can delay
  polling, just as they delay reconciliation; readiness is not proof that a command executed.

## Evidence and remaining acceptance

The fake-GitHub lease suite covers follower refusal, redelivery of the same signed edit to the
leader, duplicate suppression, live-leader inbox polling and recovery of legacy follower rows
on takeover. Router unit tests cover startup refusal and readiness changes without GitHub I/O.
The existing landing/security suites cover base moves and stop conditions during failed
attempts. Keep the sandbox comparison, real-ingress failover verification, two-week shadow
acceptance and independent feature review; this decision does not mark any feature conformant.

Sources for the comparison (behaviour summarised in our own words):

- [GitHub merge API](https://docs.github.com/en/rest/pulls/pulls#merge-a-pull-request)
- [Mergify lifecycle](https://docs.mergify.com/merge-queue/lifecycle/)
- [Mergify deployment modes](https://docs.mergify.com/merge-queue/deploy/)
- [GitHub failed webhook deliveries](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries)
