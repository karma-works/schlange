# Arztagent Drop-In Completion Roadmap

This reference maps the concrete engineering, verification, and operational steps remaining to replace Mergify on `karma-works/arzt-agent`, linking open pull requests (`#20`, `#21`, `#23`, `#24`) and tracked GitHub issues (`#25`–`#31`).

## Table of Contents
1. [In-Flight Pull Requests to Reconcile First](#1-in-flight-pull-requests-to-reconcile-first)
2. [Tracked Implementation Issues (#25–#31)](#2-tracked-implementation-issues-2531)
3. [Dependency Graph & Execution Order](#3-dependency-graph--execution-order)
4. [Arztagent Touchpoints at Cutover (Read-Only Reference)](#4-arztagent-touchpoints-at-cutover-read-only-reference)

---

## 1. In-Flight Pull Requests to Reconcile First

Before starting new branches on overlapping files, check `gh pr list --repo karma-works/schlange`:

| PR | Scope | Architectural Relevance |
|---|---|---|
| **#23** | Re-pins Nov 2026 Mergify sources (`841 -> 863` inventory items; removes `merge_queue.batch_pull_request` & `merge_queue.resolve_conflicts` from file schema) | Unblocks `cargo test -p schlange-config --test golden` and `mergify-drift` CI job on `main`. |
| **#21** | Syncs `crates/schlange-config/tests/golden/arztagent.yml` with Arztagent `main` (`batch_size: 3`, `batch_max_wait_time: 2 minutes`, informational Journeys negative lookahead, seven `-check-failure = ci-failed-<package>` merge conditions) | Keeps golden config and scenarios aligned with Arztagent's live `.mergify.yml`. |
| **#24** | Matches `check-*` regex (`~=`) and glob (`*=`) against both `<name>` and `@<app-slug>/<name>` in `schlange-core::eval` | Required for Arztagent's `-check-failure ~= ^(?!(?:@github-actions/)?verify-arztagent journeys$)` semantics. |
| **#20** | Adds opt-in sandbox shadow runner on `queue-lab` and enforces `actions: read` for `check-identity` across manifests, `permissions.rs`, token downscoping, and `doctor` | Prevents HTTP `403` on `/actions/runs?head_sha` in both active and shadow deployments; provides the harness to validate issue #25. |

---

## 2. Tracked Implementation Issues (#25–#31)

### Issue #25 — Shadow observer accuracy (`crates/schlange-github/src/shadow.rs`, `S-SHADOW`)
- **Root causes:**
  1. `shadow.rs:536–547` records `MERGIFY` `"queued"` whenever *any* check run from `mergify` exists on an open PR — matching `Summary` and `Rule: …` runs on unqueued/draft/failing PRs.
  2. `shadow.rs:114–138, 505–534, 566–595` (`embarked`) scrapes all `#<digits>` tokens from Mergify PR titles/bodies without checking `mergify/merge-queue/*` branch prefix, polluting `OtherDraft.prs` when PR titles mention issue numbers (breaking virtual-car check borrowing and emitting bogus `queued`/`batch` facts).
  3. `shadow.rs:644–691` skips closed PRs (`!p.closed` drops `PR_CLOSED`), mixes `c.seq` and `r.id` in `seen_runs`, and assumes literal `SCREAMING_SNAKE_CASE` codes in check-run prose.
- **Verification:** `cargo test -p schlange-github-fake --test shadow` + `sandbox.yml` shadow run on `queue-lab`.

### Issue #26 — Engine & GitHub adapter parity gaps (`MQ-13`, `MQ-15`, inbox crash boundary)
- **Scope:**
  1. Reconcile & land PRs #21, #23, #24 (plus a dedicated sim scenario for early `ci-failed-<package>` draft dequeue).
  2. **MQ-15 (`queue-branch-protection.md`):** Carry optional `integration_id` / app pinning from `GET /rules/branches/{branch}` into `Event::RequiredChecks` and `I1` so spoofed same-name checks from another integration cannot satisfy ruleset-injected checks.
  3. **MQ-13 (`queue-branch-protection.md`):** Refresh required checks immediately upon `repository_ruleset` / `branch_protection_rule` webhooks instead of waiting up to `--reconcile-every`.
  4. **Crash-boundary test (`github-integration.md`):** End-to-end test in `crates/schlange-github-fake/tests/serve.rs` killing/stopping the worker between HTTP `200` (`inbox_put` of an edited checkbox comment) and processing, proving restart replays the checkbox command with the webhook `sender` preserved.

### Issue #27 — `schlange doctor` ruleset accuracy & `scripts/schlange-setup.sh` cutover
- **Scope:**
  1. Filter `ruleset_check` in `crates/schlange-github/src/doctor.rs` to active branch rulesets whose `conditions.ref_name` applies to the repository's default/queue base branches (resolving the conflict with `schlange-setup.sh`'s `remove_elsewhere`).
  2. Fail (`FAIL`, exit `1`) instead of warning (`WARN`, exit `0`) when a base-branch ruleset grants only `bypass_mode: "always"` instead of `"exempt"` (resolves `.critic/github-integration.json` major finding).
  3. Fix unparsed `--yes` flag in `scripts/schlange-setup.sh` and add safe cutover migration for Arztagent's existing `Merge nur über Mergify` update-lock ruleset (avoiding dual writers or a conflicting second `Merge only via Schlange` lock).

### Issue #28 — Self-contained `serve` config validation & hardened VPS deployment stack
- **Scope:**
  1. Add an embedded clean-room structural validator for `schlange serve` and container `doctor` when `--schema` / `SCHLANGE_SCHEMA` is absent, removing the runtime volume dependency on Mergify's proprietary `schema.json` while keeping external schema validation in `config validate` and CI `mergify-drift`.
  2. Provide `ops/` (Compose/systemd, read-only rootfs, `cap_drop: [ALL]`, file-mounted secrets, TLS reverse-proxy routing webhooks via `GET /readyz`, monitoring on `/healthz` + `/status.halted`, and online SQLite WAL backup).

### Issue #29 — Sandbox baselines `S13–S18` & Critic conformance (`sandbox-e2e` + `critic.sh`)
- **Scope:**
  1. Refresh sandbox fixture on `queue-lab` and `schlange-e2e-isolated`.
  2. Record reviewed `outcome.json` baselines on `queue-lab` for `s14–s18` (replacing provisional `s11`) and an app-qualified check regex scenario.
  3. Resolve `S13` (`PR_DEQUEUED` vs `CONFLICT_WITH_BASE_BRANCH`) in `schlange-core` test-first (`scenarios/`).
  4. Achieve green active (`schlange-e2e-isolated`) and shadow (`queue-lab`) runs, capture `verify-schlange` evidence, and record `conformant` verdicts via `scripts/critic.sh`.

### Issue #31 — 14-day Arztagent shadow run & guarded production cutover
- **Phase A (Shadow):** Run read-only `Schlange Schatten Arztagent` (`metadata`, `contents`, `issues`, `pull_requests`, `checks`, `statuses`, `actions`: `read`) on self-hosted VPS for 14 consecutive days with `0` disagreements in `schlange shadow report`.
- **Phase B (Cutover):** Human constitution PR in `arzt-agent`, `workflows: write` on active app, `schlange-setup.sh` single-writer lock cutover, Mergify paused as fallback.

### Issue #30 — Post-cutover `S-MCP` server (`P2`, non-blocking for initial cutover)
- Implement `schlange mcp` (`stdio` + authenticated HTTP) exposing `queue_status`, `pr_explain`, `config_validate`, `config_simulate`, `events`, `audit`, and `commands_restrictions`-enforced `enqueue`/`dequeue` within `image-budget.txt`.

---

## 3. Dependency Graph & Execution Order

```text
PRs #20, #21, #23, #24
   ├──► #25 (Shadow observer fixes) ────────┐
   ├──► #26 (MQ-13, MQ-15, inbox crash E2E) ├──► #29 (Sandbox S13–S18 + Critic conformance)
   └──► #27 (Doctor rulesets + setup script)┘                │
                                                             ▼
        #28 (Schema-free serve + VPS ops stack) ────► #31 Phase A: 14-day Arztagent shadow run
                                                             │
                                                             ▼
                                                      #31 Phase B: Guarded Arztagent cutover
                                                             │
                                                             ▼
                                                      #30 S-MCP (Phase 2 Agent-First surface)
```

---

## 4. Arztagent Touchpoints at Cutover (Read-Only Reference)

> **Never modify `~/Projects/arzt-agent` from a Schlange agent session (`AGENTS.md`).** These notes exist solely so architectural decisions in Schlange stay compatible with Arztagent's workflows and governance.

- **Unchanged in `arzt-agent`:**
  - `.mergify.yml` (100% compatible; Schlange reads it from `main`).
  - `.github/workflows/ci.yml` & package quality workflows (key off `mergify/merge-queue/*` branch prefix, which Schlange preserves).
  - `.github/workflows/ai-review.yml` (skips drafts and `mergify/merge-queue/*` branches).
  - `governance/mergify_config.py` & `governance/tests/test_mergify.py` (generate and verify the Python negative-lookahead constitution regex).
- **Requires human constitution PR / admin action at cutover (#31):**
  1. `.github/workflows/pr-gate.yml` (line 28): add `<schlange-app-slug>[bot]` alongside `mergify[bot]` in `QUEUE` so queue draft PRs skip the single-PR risk/data-protection gate.
  2. `scripts/agent-first-preflight.sh` (line 91): recognize the Schlange GitHub App slug.
  3. Active GitHub App permissions: include **`workflows: write`** so human-queued constitution PRs touching `.github/workflows/*.yml` can be squash-merged by Schlange without HTTP `403`.
  4. Ruleset lock `Merge nur über Mergify`: migrate via `scripts/schlange-setup.sh` (#27) so only the active queue app holds `exempt` bypass on `main`.
