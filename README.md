<p align="center">
  <img src="docs/assets/logo.svg" alt="" width="112" height="112">
</p>

<h1 align="center">Schlange</h1>

<p align="center">
  An open-source, self-hosted merge queue that reads your existing <code>.mergify.yml</code>.
  <br>
  <a href="https://karma-works.github.io/schlange/">Website</a> ·
  <a href="KONZEPT.md">Concept</a> ·
  <a href="#implementation-status">Status</a>
</p>

---

**Schlange** (German for both *snake* and *queue*) tests pull requests together with the
current `main` before they land, so `main` stays green. It is built as a drop-in replacement for
Mergify's merge queue for one concrete project, Arztagent, whose `.mergify.yml` must keep
working byte for byte.

> **Status: alpha.** The engine, the simulated and fake-GitHub test layers and shadow mode are
> built; no feature is accepted as conformant yet, and Schlange has not merged a pull request in
> a production repository. Do not let it land changes on a branch you care about. Run it in
> shadow mode next to your current merge queue first, as described in
> [Adopting Schlange](#adopting-schlange).

## Why

- **Own the critical path.** A merge queue decides what reaches `main`. Running it yourself keeps
  code and pull request data in house and removes a dependency on a hosted service's outages.
- **Small and hardened.** One static binary in a `FROM scratch` image (20 MB budget), read-only
  file system, no shell, no git, SQLite on one volume.
- **Explainable.** `schlange explain <pr>` says why a pull request is or is not queued,
  condition by condition; every merge has an audit record (tested tree, base, heads, landed
  commit).
- **Agent first.** Every function is a CLI command with `--json` and stable exit codes.
- **Correct by construction, then by evidence.** A side-effect-free core
  `(state, event) → (state, decisions)`, invariants I0–I5 checked after every simulated step
  (for example: every commit on `main` has a tree that was tested green), property tests with
  injected faults and crashes.

## How it works

```
GitHub webhooks ─┐                         ┌─ create queue branch, open draft PR
reconciliation ──┼─▶ core state machine ──▶├─ land with an expected head, verify the tree
                 │   (no I/O, journaled)   └─ labels, comments, check runs (cosmetic)
.mergify.yml ────┘
```

Webhooks are hints, reconciliation is the truth; every side effect is journaled first; one
writer per repository (a lease plus a fence ref on GitHub). Details: [KONZEPT.md](KONZEPT.md)
(German) and [AGENTS.md](AGENTS.md).

## Quick start

```bash
tools/fetch-mergify-sources                                 # pinned Mergify schema (not redistributed)
cargo build --release -p schlange

target/release/schlange config validate .mergify.yml       # 0 valid, 1 invalid, 2 valid but unsupported
target/release/schlange sim run scenarios/p1-bisection.yaml # a scenario in the simulated forge
target/release/schlange explain 2 --scenario scenarios/p0-queue-conditions.yaml
target/release/schlange doctor
```

Running against GitHub as an app (`schlange serve`, flags and security notes) and shadow mode
(`--shadow`, `schlange shadow report`) are described in [AGENTS.md](AGENTS.md#schlange-serve).
The `/healthz` endpoint returns 503 if a worker stops or its loop heartbeat is older than
30 minutes (or three times the worker poll interval, whichever is longer). A lengthy API
operation or reconciliation can also trigger this alarm. It clears when the worker resumes;
it does not restart the process or detect every stalled queue. A liveness probe may restart
the process on 503: configure its failure threshold to allow for the longest expected operation,
or use the endpoint for alerting and investigate before restarting.

For an active leader, a separate read-only observer samples one eligible car at most once per
minute, rotating past cars that are not actionable. It reads that car's pull requests and draft,
their checks and the relevant branches, so API cost grows with the size of the car. If two
completed observations at least a minute apart find the same green car ready for verification, it requests an ordinary
reconciliation; the worker alone decides and journals any writes. The observer discards failed
reads and observations made against a changed engine view. Its reads use background priority and
leave the worker's local API reserve available; GitHub's shared upstream rate limit still applies.
A budget-deferred observation resumes successful GETs, including pages, for up to ten minutes,
provided the engine view and leadership do not change. It is not an atomic GitHub snapshot; the
worker makes its own fresh reads before landing. Memoized keys, headers and bodies are limited to
2 MiB and 512 entries per observation and 16 MiB across the service process; decoded pull requests
and map overhead are outside those limits. A larger observation is discarded. With many eligible
cars, rotation can also exceed the ten-minute age limit before a partial read is revisited. It
does not observe shadow processes or lease followers, and detects this particular stalled state
rather than every kind of stall. Frequent unrelated engine state changes can also prevent two
observations from sharing a generation; the observer rotates among eligible cars in that case.

## Adopting Schlange

A merge queue decides what reaches `main`, so switch the way we do it for Arztagent: watch
first, compare, then hand over, and keep the old queue as the way back.

1. **Validate your configuration.** `schlange config validate --strict .mergify.yml`. Exit 2
   names every option Schlange does not support yet; resolve those before going further.
2. **Shadow mode.** Install a GitHub App with *read-only* permissions and run
   `schlange serve --shadow` next to your current merge queue. Schlange decides as it would,
   writes nothing, and records what the other queue actually did. Read-only permissions make
   this safe even if a bug or a missing flag tried to write. Nothing in your repository changes.
3. **Compare.** `schlange shadow report` lists every pull request where Schlange decided
   differently: queued or not, landed or not, landing order, the batch it landed in, the
   dequeue reason (exit 1 on any disagreement).
   Reproduce each difference as a scenario, fix it with a test, and keep comparing. Our bar:
   two weeks of normal traffic without a differing decision.
4. **Switch.** Grant the app write permissions, add it as a bypass actor of the branch
   rulesets, let workflows that recognise the queue's bot accept Schlange's, then pause the
   old queue.
5. **Keep the way back.** Leave the old queue installed but paused until Schlange has landed
   reliably for a few weeks.

Step by step, for any repository (app, container, webhooks, `doctor --shadow`, report):
[docs/shadow-mode.md](docs/shadow-mode.md). The runbook for Arztagent:
[docs/betrieb/schattenbetrieb.md](docs/betrieb/schattenbetrieb.md). The same comparison runs
continuously in the [GitHub sandbox](docs/betrieb/sandbox.md) against Mergify.

## Testing

| Layer | What it proves |
|---|---|
| Unit and golden files | condition parser, check semantics, commit messages, explanations |
| Property tests | invariants I0–I5 over random event sequences, with forge faults and crashes |
| Simulated forge | 40+ scenarios with a virtual clock, webhook loss, reordering and delays |
| Real git (`localgit`) | merge methods and batch sizes on real repositories |
| Fake GitHub | `schlange serve` over HTTP: signed webhooks, API faults, an attacker with write access |
| [GitHub sandbox](docs/betrieb/sandbox.md) | real GitHub and Mergify side by side; Mergify's behaviour as baselines |

The problem catalogue ([docs/research/merge-queue-probleme.md](docs/research/merge-queue-probleme.md),
MQ-01…MQ-53) lists known merge queue failure modes; [docs/mq-coverage.md](docs/mq-coverage.md)
maps each to its tests.

Agents verify with two skills: [`verify-schlange`](.agents/skills/verify-schlange/SKILL.md)
(build, doctor, drive, evidence) and [`sandbox-e2e`](.agents/skills/sandbox-e2e/SKILL.md)
(end-to-end on real GitHub).

## Implementation status

<!-- feature-status:start -->
<!-- generated by scripts/feature-status.py from the feature map; do not edit by hand -->

Every Mergify feature has a file in the feature map ([`.agents/skills/verify-schlange/features/`](.agents/skills/verify-schlange/features/)). A feature is **conformant** only after a green verification and an independent critic verdict; **partial** means some of its sub-features are built; **missing** means none.

| | conformant | partial | missing |
|---|---:|---:|---:|
| Features (86) | 0 | 73 | 13 |
| Sub-features (307) | – | built 256, partial 14 | 37 |
| Sub-features Arztagent uses (167) | – | built 149, partial 9 | 9 |

<details>
<summary><b>P1 – what Arztagent uses</b>: 36 features (0 conformant, 36 partial, 0 missing)</summary>

| Feature | Status | Sub-features built / partial / missing | Arztagent |
|---|---|---|---|
| [Evidence per merge](.agents/skills/verify-schlange/features/S-AUDIT.md) | partial | 2 / 0 / 0 |  |
| [Complete CLI with `--json`](.agents/skills/verify-schlange/features/S-CLI.md) | partial | 4 / 1 / 0 |  |
| [`schlange doctor`](.agents/skills/verify-schlange/features/S-DOCTOR.md) | partial | 10 / 0 / 0 |  |
| [`schlange explain`](.agents/skills/verify-schlange/features/S-EXPLAIN.md) | partial | 6 / 0 / 2 |  |
| [Shadow mode](.agents/skills/verify-schlange/features/S-SHADOW.md) | partial | 6 / 0 / 1 |  |
| [Scenarios in the simulated forge](.agents/skills/verify-schlange/features/S-SIM.md) | partial | 7 / 0 / 0 |  |
| [Action `queue`](.agents/skills/verify-schlange/features/action-queue.md) | partial | 4 / 0 / 0 | yes |
| [Commands `queue`, `dequeue`, `refresh`](.agents/skills/verify-schlange/features/commands.md) | partial | 9 / 0 / 0 | yes |
| [Who may run commands (`commands_restrictions`)](.agents/skills/verify-schlange/features/commands-restrictions.md) | partial | 7 / 0 / 0 | yes |
| [Attributes `base`, `draft`, `label`, `author`, `sender`](.agents/skills/verify-schlange/features/conditions-basic.md) | partial | 5 / 0 / 1 | yes |
| [Check attributes and check semantics](.agents/skills/verify-schlange/features/conditions-checks.md) | partial | 4 / 0 / 0 | yes |
| [Dependabot attributes](.agents/skills/verify-schlange/features/conditions-dependabot.md) | partial | 1 / 0 / 0 | yes |
| [Attribute `files` with regex](.agents/skills/verify-schlange/features/conditions-files.md) | partial | 4 / 1 / 0 | yes |
| [Condition syntax and combinators](.agents/skills/verify-schlange/features/conditions-syntax.md) | partial | 2 / 0 / 0 | yes |
| [Find and load the configuration file](.agents/skills/verify-schlange/features/config-file.md) | partial | 4 / 0 / 1 | yes |
| [Validate a configuration](.agents/skills/verify-schlange/features/config-validation.md) | partial | 1 / 0 / 0 | yes |
| [Dependencies between pull requests (`Depends-On`)](.agents/skills/verify-schlange/features/depends-on.md) | partial | 7 / 0 / 0 | yes |
| [GitHub integration (app, webhooks, reconciliation)](.agents/skills/verify-schlange/features/github-integration.md) | partial | 18 / 3 / 0 | yes |
| [Built-in protection: configuration changed](.agents/skills/verify-schlange/features/merge-protections-builtin.md) | partial | 6 / 0 / 0 | yes |
| [Workflow rules (`pull_request_rules`)](.agents/skills/verify-schlange/features/pull-request-rules.md) | partial | 9 / 0 / 0 | yes |
| [Batches (`batch_size`, `batch_max_wait_time`)](.agents/skills/verify-schlange/features/queue-batch.md) | partial | 3 / 1 / 1 | yes |
| [Bisection of red batches](.agents/skills/verify-schlange/features/queue-bisection.md) | partial | 6 / 0 / 0 | yes |
| [Bot accounts for merge, draft and update](.agents/skills/verify-schlange/features/queue-bot-accounts.md) | partial | 1 / 0 / 0 | yes |
| [Foreign pushes to queue branches](.agents/skills/verify-schlange/features/queue-branch-edit.md) | partial | 3 / 0 / 0 | yes |
| [Required checks from branch protection and rulesets](.agents/skills/verify-schlange/features/queue-branch-protection.md) | partial | 3 / 1 / 1 | yes |
| [Check timeout and retries (defaults)](.agents/skills/verify-schlange/features/queue-checks-timeout.md) | partial | 7 / 0 / 0 | yes |
| [Commit message on merge](.agents/skills/verify-schlange/features/queue-commit-message.md) | partial | 4 / 0 / 1 | yes |
| [Conflicts and a moving base](.agents/skills/verify-schlange/features/queue-conflicts.md) | partial | 4 / 1 / 0 | yes |
| [Dequeue with a reason](.agents/skills/verify-schlange/features/queue-dequeue-reasons.md) | partial | 6 / 0 / 0 | yes |
| [Queue pull requests as drafts on `mergify/merge-queue/`](.agents/skills/verify-schlange/features/queue-draft-pr.md) | partial | 4 / 0 / 1 | yes |
| [Queue labels and status comments](.agents/skills/verify-schlange/features/queue-labels-comments.md) | partial | 4 / 0 / 0 | yes |
| [Landing as compare-and-swap with evidence](.agents/skills/verify-schlange/features/queue-landing.md) | partial | 6 / 2 / 0 | yes |
| [Merge method `squash`](.agents/skills/verify-schlange/features/queue-merge-method-squash.md) | partial | 3 / 0 / 2 | yes |
| [Queue rules (`queue_rules`)](.agents/skills/verify-schlange/features/queue-rules.md) | partial | 5 / 0 / 1 | yes |
| [Serial, cumulative speculation](.agents/skills/verify-schlange/features/queue-speculation.md) | partial | 4 / 0 / 0 | yes |
| [Two-step CI (`queue_conditions` and `merge_conditions`)](.agents/skills/verify-schlange/features/queue-two-step.md) | partial | 5 / 0 / 0 | yes |

</details>

<details>
<summary><b>P2 – the rest of Mergify's merge queue and rules</b>: 45 features (0 conformant, 37 partial, 8 missing)</summary>

| Feature | Status | Sub-features built / partial / missing | Arztagent |
|---|---|---|---|
| [`schlange config simulate`](.agents/skills/verify-schlange/features/S-SIMULATE.md) | partial | 1 / 0 / 1 |  |
| [Action `assign`](.agents/skills/verify-schlange/features/action-assign.md) | partial | 2 / 0 / 0 |  |
| [Action `backport`](.agents/skills/verify-schlange/features/action-backport.md) | partial | 2 / 0 / 0 |  |
| [Action `close`](.agents/skills/verify-schlange/features/action-close.md) | partial | 1 / 0 / 0 |  |
| [Action `comment`](.agents/skills/verify-schlange/features/action-comment.md) | partial | 1 / 0 / 0 |  |
| [Action `copy`](.agents/skills/verify-schlange/features/action-copy.md) | partial | 1 / 0 / 0 |  |
| [Action `delete_head_branch` (deprecated)](.agents/skills/verify-schlange/features/action-delete-head-branch.md) | partial | 1 / 0 / 0 |  |
| [Action `dismiss_reviews`](.agents/skills/verify-schlange/features/action-dismiss-reviews.md) | partial | 2 / 0 / 0 |  |
| [Action `edit`](.agents/skills/verify-schlange/features/action-edit.md) | partial | 1 / 0 / 0 |  |
| [Action `github_actions` (deprecated)](.agents/skills/verify-schlange/features/action-github-actions.md) | partial | 1 / 1 / 0 |  |
| [Action `label`](.agents/skills/verify-schlange/features/action-label.md) | partial | 3 / 0 / 0 |  |
| [Action `merge`](.agents/skills/verify-schlange/features/action-merge.md) | partial | 1 / 0 / 0 |  |
| [Action `post_check`](.agents/skills/verify-schlange/features/action-post-check.md) | partial | 2 / 0 / 0 |  |
| [Action `rebase`](.agents/skills/verify-schlange/features/action-rebase.md) | partial | 1 / 0 / 1 |  |
| [Action `request_reviews`](.agents/skills/verify-schlange/features/action-request-reviews.md) | partial | 1 / 0 / 1 |  |
| [Action `review`](.agents/skills/verify-schlange/features/action-review.md) | partial | 1 / 0 / 0 |  |
| [Action `squash`](.agents/skills/verify-schlange/features/action-squash.md) | partial | 1 / 0 / 0 |  |
| [Action `update`](.agents/skills/verify-schlange/features/action-update.md) | partial | 1 / 0 / 0 |  |
| [Commands `backport`, `copy`, `rebase`, `squash`, `update`](.agents/skills/verify-schlange/features/commands-other.md) | partial | 5 / 0 / 0 |  |
| [Further pull request metadata](.agents/skills/verify-schlange/features/conditions-pr-metadata.md) | partial | 0 / 0 / 0 |  |
| [Queue attributes](.agents/skills/verify-schlange/features/conditions-queue.md) | partial | 3 / 0 / 0 |  |
| [Shared defaults (`defaults`)](.agents/skills/verify-schlange/features/config-defaults.md) | partial | 1 / 0 / 1 |  |
| [Inherit a configuration (`extends`)](.agents/skills/verify-schlange/features/config-extends.md) | partial | 2 / 0 / 1 |  |
| [Shared anchors (`shared`)](.agents/skills/verify-schlange/features/config-shared.md) | partial | 1 / 0 / 0 |  |
| [Configuration simulator](.agents/skills/verify-schlange/features/config-simulator.md) | partial | 1 / 0 / 1 |  |
| [Direct merge without queue CI](.agents/skills/verify-schlange/features/direct-merge.md) | partial | 2 / 0 / 1 |  |
| [Event log](.agents/skills/verify-schlange/features/events.md) | partial | 1 / 0 / 1 |  |
| [Built-in protection `Merge-After`](.agents/skills/verify-schlange/features/merge-after.md) | partial | 3 / 0 / 1 |  |
| [Merge protections and auto-merge](.agents/skills/verify-schlange/features/merge-protections.md) | partial | 1 / 1 / 1 |  |
| [Scheduled freeze](.agents/skills/verify-schlange/features/queue-freeze.md) | partial | 4 / 1 / 2 |  |
| [Other merge methods](.agents/skills/verify-schlange/features/queue-merge-methods-other.md) | partial | 5 / 0 / 0 |  |
| [Queue modes `parallel` and `isolated`](.agents/skills/verify-schlange/features/queue-modes.md) | partial | 3 / 1 / 1 |  |
| [Pause the queue](.agents/skills/verify-schlange/features/queue-pause.md) | partial | 5 / 0 / 1 |  |
| [Priorities (`priority_rules`)](.agents/skills/verify-schlange/features/queue-priority.md) | partial | 5 / 0 / 1 |  |
| [Skip intermediate results](.agents/skills/verify-schlange/features/queue-skip-intermediate.md) | partial | 1 / 0 / 2 |  |
| [Queue status through API and CLI](.agents/skills/verify-schlange/features/queue-status-api.md) | partial | 1 / 0 / 1 |  |
| [Scopes for monorepos](.agents/skills/verify-schlange/features/scopes.md) | partial | 3 / 0 / 4 |  |
| [MCP server](.agents/skills/verify-schlange/features/S-MCP.md) | missing | 0 / 0 / 0 |  |
| [CI helpers for queue runs](.agents/skills/verify-schlange/features/ci-helpers.md) | missing | 0 / 0 / 0 |  |
| [Deployment attributes](.agents/skills/verify-schlange/features/conditions-deployments.md) | missing | 0 / 0 / 0 |  |
| [Further file and line attributes](.agents/skills/verify-schlange/features/conditions-files-other.md) | missing | 0 / 0 / 0 |  |
| [Review attributes](.agents/skills/verify-schlange/features/conditions-reviews.md) | missing | 0 / 0 / 0 |  |
| [Time and schedule](.agents/skills/verify-schlange/features/conditions-time.md) | missing | 0 / 0 / 0 |  |
| [Conflict resolvers (experimental)](.agents/skills/verify-schlange/features/conflict-resolvers.md) | missing | 0 / 0 / 1 |  |
| [Update method of a pull request](.agents/skills/verify-schlange/features/queue-update-method.md) | missing | 1 / 0 / 2 |  |

</details>

<details>
<summary><b>P3 – other Mergify products</b>: 5 features (0 conformant, 0 partial, 5 missing)</summary>

| Feature | Status | Sub-features built / partial / missing | Arztagent |
|---|---|---|---|
| [CI Insights](.agents/skills/verify-schlange/features/ci-insights.md) | missing | 0 / 0 / 0 |  |
| [Dashboard and statistics](.agents/skills/verify-schlange/features/dashboard-statistics.md) | missing | 0 / 0 / 0 |  |
| [Integrations, products and account](.agents/skills/verify-schlange/features/integrations-account.md) | missing | 0 / 0 / 0 |  |
| [Stacks (stacked pull requests)](.agents/skills/verify-schlange/features/stacks.md) | missing | 0 / 0 / 0 |  |
| [Test Engine (flaky detection, quarantine)](.agents/skills/verify-schlange/features/test-engine.md) | missing | 0 / 0 / 0 |  |

</details>

<!-- feature-status:end -->

## Clean room

Schlange is not affiliated with Mergify. Its behaviour comes only from Mergify's public
documentation, its published configuration schema and observed behaviour, written in our own
words. No Mergify code or documentation text is in this repository, and the Mergify CLI is not
used. "Mergify" is a trademark of its owner.

## Contributing

Read [AGENTS.md](AGENTS.md) first: the rules (clean room, test first, invariants, evidence,
critic acceptance) apply to people and agents alike. Work on a branch and open a pull request.

## License

[Apache License 2.0](LICENSE). See [NOTICE](NOTICE).
