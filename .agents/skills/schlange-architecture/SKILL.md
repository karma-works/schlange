---
name: schlange-architecture
description: "Guide architectural decisions, crate boundaries, invariant and security trade-offs, and the phased engineering plan for completing Schlange as a drop-in Mergify replacement for Arztagent. Make sure to use this skill whenever an architectural or design decision comes up in Schlange — including deciding where logic belongs (schlange-core vs. schlange-github vs. schlange-config), evaluating GitHub API atomicity or permission limits against invariants I0–I5, prioritizing features (Phase 1 Arztagent parity vs. Phase 2/3), designing shadow-mode or ruleset safeguards, handling clean-room or container constraints, or planning what to build next to finish replacing Mergify in Arztagent, even if the user does not explicitly say 'architecture'."
---

# Schlange Architecture & Arztagent Drop-In Completion Guide

Schlange replaces Mergify for `karma-works/arzt-agent` without changing Arztagent's `.mergify.yml`, running self-hosted inside a hardened `FROM scratch` container.

Arztagent previously replaced ~8,600 lines of custom merge-controller scripts because reactive edge-case patches became unmaintainable (`KONZEPT.md` §1). Every architectural decision in Schlange exists to prevent repeating that failure mode: **strict scope discipline, a side-effect-free state machine verified under fault injection, explicit holds, and two weeks of zero-deviation shadow evidence before touching `main`.**

For the concrete remaining backlog, open PR coordination, and issue-by-issue implementation order, read [`references/arztagent-completion-roadmap.md`](references/arztagent-completion-roadmap.md).

---

## 1. The Scope Gate: Does Arztagent Need It Now?

Mergify's schema spans 860+ inventory entries across CI Insights, Stacks, Test Engine, and 18 rule actions. Arztagent uses a narrow, well-defined slice (`KONZEPT.md` §3, `crates/schlange-config/tests/golden/arztagent.yml`).

Before designing or expanding any feature, classify it:

1. **Phase 1 — Arztagent Parity (build, verify, get critic conformance now):**
   - Top-level sections: `queue_rules`, `pull_request_rules` (`queue` action), `commands_restrictions`.
   - Queue behaviour: `merge_method: squash`, batches (`batch_size`, `batch_max_wait_time`), two-step CI via draft PRs on `mergify/merge-queue/<10 hex>`, serial cumulative speculation (`max_parallel_checks: 5`), red-batch bisection, fast-fail `-check-failure` conditions, sticky dequeue on broken `queue_conditions`, built-in `Depends-On` and config-changed holds.
   - Attributes & operators: `base`, `draft`, `label`, `check-success`, `check-failure` (including `#` count and `~=` Python lookahead regex against both `<name>` and `@<app-slug>/<name>`), `author`, `files` (`~=` lookahead), `dependabot-update-type`, `sender`.
   - Operational core: `S-DOCTOR`, `S-SHADOW`, `S-EXPLAIN`, `S-AUDIT`, single-writer lease + GitHub ref fence, durable SQLite journal & webhook inbox, check-identity verification, watchdog.
2. **Phase 2 / Phase 3 — Post-Cutover (`S-MCP`, extra actions, `parallel`/`isolated` modes, `W-*` enhancements):**
   - Never block Arztagent's shadow run or cutover on Phase 2/3 work.
   - Keep unsupported Mergify options mapped in `.agents/skills/verify-schlange/features/` so `schlange config validate --strict` reports them cleanly with exit code `2` instead of silently ignoring them.

---

## 2. Crate Boundaries and Placement Rules

| Layer | Crate | Allowed Responsibilities | Forbidden |
|---|---|---|---|
| **Configuration & Support Matrix** | `schlange-config` | YAML loading, schema/structural check, condition & template parsing (`fancy-regex`), semantic checks, feature-map support reporting | Network I/O, mutable state, reading unpinned live docs at runtime |
| **Deterministic Engine** | `schlange-core` | Pure `(State, Event) -> (State, Vec<Decision>)`, condition evaluation, car scheduling/bisection, command authorisation, `State::holds`, `explain`, journal model | Clocks, sockets, filesystem, SQLite, HTTP status codes, retries, sleeping |
| **Forge Model & Verification** | `schlange-forge` | `Forge` trait, in-memory `sim` forge, `localgit` forge, fault/crash injection, invariant checker `I0–I5`, property tests | Weakening `I0–I5` for adapter quirks; calling external networks |
| **GitHub Production Adapter** | `schlange-github` | App JWT & downscoped tokens, bounded HTTP listener, durable webhook inbox, reconciliation, `GithubForge`, check-identity gate, lease/fence, shadow overlay, SQLite WAL store, `doctor` | Retrying failed/rate-limited writes behind the engine's back; mutating GitHub in `--shadow` or after losing the lease |
| **HTTP Test Harness** | `schlange-github-fake` | Signed webhooks, REST simulation, read/mergeable lag, attacker fuzzing, scripted reference queue for shadow tests | Calling `api.github.com` or using real secrets |
| **CLI & Entrypoint** | `crates/schlange` | Thin command-line and admin API wiring (`--json`, stable exit codes `0/1/2/3/64`) | Embedding queue policy or bypassing engine decisions |

### Key placement heuristics
- **If a rule depends on time, retries, or ordering of queue events:** put it in `schlange-core` driven by `Time` and explicit `Event`s so `sim` and `localgit` property tests exercise it across crashes.
- **If GitHub returns an ambiguous or transient failure (`405`, `409`, `429`, `5xx`, timeout, `mergeable: null`):** resolve what actually happened via fresh reads in `schlange-github` (for `5xx`/timeouts, `MQ-35`) and report `Failed` / fresh state back to `schlange-core`. Never loop a write retry inside the HTTP client or adapter: a `halt` label or base push during the wait would otherwise be missed.
- **Every reason an open PR is not queued must be a `Hold` in `State::holds`:** whether it is a built-in protection, sticky dequeue (`dequeued-waits-for-new-head`), missing check provenance (`check-identity-unavailable`), or a waiting command. `schlange explain` must surface it.
- **Cosmetics (`Decision::Cosmetic`) are write-only:** labels, status comments, reactions, and Schlange's own check run (`Schlange merge queue`) never feed back into condition evaluation and yield rate-limit budget to reads and landings.

---

## 3. Model Invariants vs. GitHub's Last-Request Window

Follow [`docs/betrieb/arztagent-parity.md`](../../../docs/betrieb/arztagent-parity.md) whenever an architectural question pits `I0`/`I1` against what GitHub's REST API exposes:

1. **Never weaken the model invariants (`I0–I5`).** `schlange-core`, `schlange-forge`, and `localgit` keep strict `I0–I5` checking after every step.
2. **Do not invent broken workarounds for missing GitHub Merge CAS.** GitHub's `PUT /pulls/{n}/merge` binds `sha` (the PR head) but has no expected-base SHA parameter and cannot atomically condition a merge on the absence of a concurrent label edit. Updating `refs/heads/main` directly via Git Data API would give CAS on the base ref, but GitHub then marks PRs *closed* instead of *merged* and requires bypassing every PR rule. Therefore:
   - Keep `PUT /pulls/{n}/merge` with `sha = tested_head`.
   - Perform a fresh consistent read of the PR (still open, not draft, no `halt`, at `tested_head`) and the base ref (`== expected_base`) immediately before every merge call.
   - Verify the landed commit's parent and Git tree immediately afterwards against the car's persisted prefix tree; halt the queue at once on mismatch and record the `S-AUDIT` proof.
   - Enforce operational single-writer discipline on `main` via GitHub rulesets (`exempt` only for the active queue app; admins via PR).
3. **Leader-only webhook admission.** Followers, uninitialised workers, and expired leases must reject webhooks with `503` *before* SQLite dedup/inbox persistence so the `X-GitHub-Delivery` ID remains valid when retried against the leader. Ingress routes webhooks via `GET /readyz`, never `GET /healthz`.

---

## 4. Arztagent Threat Model & Security Invariants

In Arztagent, autonomous coding agents hold repository write access: they can push branches, open PRs, add/remove labels, post comments, and **edit anyone's comments**. Every design choice must remain fail-closed against a write-privileged agent:

- **Comment edits never execute text commands.** GitHub's REST comment API does not expose who edited a comment; only the `issue_comment` `edited` webhook names `sender`. Therefore reconciliation ignores edited comments, and webhook edits only count as checkbox ticks attributed to the webhook `sender` (persisted in the durable SQLite `inbox` before HTTP `200`).
- **Commit metadata never vouches for identity.** Any new head SHA cancels a waiting command; command head-binding uses GitHub's branch activity log (`GET /repos/{o}/{r}/activity`), never commit author/committer fields.
- **Truncated file lists fail closed.** GitHub's `GET /pulls/{n}/files` caps at 3,000 files. When `files.len() < changed_files`, mark `files_incomplete = true` so every `files` condition (negated or positive, including Arztagent's `-files ~= <constitution regex>`) evaluates to an error and refuses auto-queuing.
- **Check provenance fails closed.** Require `actions: read` alongside `checks: read` (active and shadow apps). Associate each check suite/run with its app slug, workflow file, and trigger event; hold affected bases (`check-identity-unavailable`) if suite/workflow provenance cannot be read.
- **Shadow mode is enforced by GitHub permissions, not just `--shadow`.** In addition to `ShadowGate` refusing non-`GET` calls in code, the Arztagent shadow app (`Schlange Schatten Arztagent`) holds read-only permissions only.
- **Container & runtime isolation.** Static musl binary (`rustls` + `ring`, bundled trimmed SQLite, no OpenSSL, no shell, no `git` CLI) in `FROM scratch` as UID `65532`. `serve` should validate configs at runtime via a clean-room structural validator so the production container does not depend on mounting Mergify's proprietary `schema.json` (while CI/dev `config validate` continues checking against the pinned Mergify schema).

---

## 5. Phased Plan to Finish Replacing Mergify in Arztagent

Execute the remaining work in this strict dependency order (details and issue links in [`references/arztagent-completion-roadmap.md`](references/arztagent-completion-roadmap.md)):

1. **Stage 1 — Reconcile in-flight parity PRs & fix Shadow, Doctor, and Setup blockers**
   - Land/reconcile PRs #20 (`actions: read`), #21 (latest Arztagent `.mergify.yml`), #23 (Nov 2026 inventory pin), and #24 (`@<app>/<name>` check regex).
   - Fix `shadow.rs` false disagreements (issue #25), `doctor.rs` base-branch ruleset filtering & `bypass_mode=always` failure + `schlange-setup.sh` Mergify lock migration (issue #27), and `MQ-13`/`MQ-15`/inbox crash-boundary tests (issue #26).
2. **Stage 2 — Sandbox differential baselines (`S13–S18`) & Critic conformance (issue #29)**
   - Record missing `queue-lab` baselines (`s14–s18`), resolve `S13` (`PR_DEQUEUED` vs `CONFLICT_WITH_BASE_BRANCH`), pass both active (`schlange-e2e-isolated`) and shadow (`queue-lab`) runs via `sandbox-e2e`, and obtain `conformant` verdicts from `scripts/critic.sh` after `verify-schlange`.
3. **Stage 3 — Self-contained `serve` validation & hardened VPS deployment stack (issue #28)**
   - Decouple `schlange serve` startup from external `.research/mergify/schema.json` and check in the hardened VPS deployment/backup/alerting bundle (`ops/`).
4. **Stage 4 — 14-day zero-deviation read-only shadow run on `karma-works/arzt-agent` (issue #31, Phase A)**
   - Run `Schlange Schatten Arztagent` on self-hosted infrastructure (never `smee.io`); review `schlange shadow report` daily. Every discrepancy becomes a `scenarios/*.yaml` regression test before fixing.
5. **Stage 5 — Human-led Arztagent cutover with Mergify as cold fallback (issue #31, Phase B)**
   - Never edit `~/Projects/arzt-agent` from a Schlange agent (`AGENTS.md`). A human lands the constitution PR in `arzt-agent` (`pr-gate.yml` bot login + `agent-first-preflight.sh`), grants `workflows: write`, migrates the `Merge nur über Mergify` lock ruleset via `schlange-setup.sh`, and pauses (without uninstalling) Mergify.
6. **Stage 6 — Post-cutover Agent-First surface (`S-MCP`, issue #30)**
   - Add `schlange mcp` (`stdio` + authenticated HTTP) enforcing `commands_restrictions` on `enqueue`/`dequeue` while staying within `image-budget.txt`.

---

## 6. Architecture Decision Checklist & Output Template

Whenever evaluating an architectural proposal, design trade-off, or tricky bug fix in Schlange, run through these seven checks before writing code:

1. **Scope:** Is this required for Arztagent Phase 1 parity or operational safety? If Phase 2/3, does deferring it keep `arztagent.yml` valid and `--strict` honest?
2. **Clean room:** Is all behaviour derived from observed sandbox runs, public schema identifiers, or our own specs — with zero Mergify code or prose?
3. **Layering:** Can the decision logic live purely in `schlange-core` as `(State, Event) -> (State, Decisions)` with a `scenarios/*.yaml` test written *before* adapter code?
4. **Invariants (`I0–I5`):** Does `I0–I5` still hold across webhook loss, reordering, `5xx` ambiguity, and crash-at-every-side-effect without weakening the invariant checker?
5. **Threat model:** Can a write-privileged agent exploit this via comment edits, forged commit authors, `>3000` files, spoofed check names/apps, or race windows during `405`/`409`/`429`?
6. **Observability:** If this blocks or delays a PR, is there an explicit `Hold` in `State::holds`, visible in `schlange explain` and `doctor`?
7. **Container & budget:** Does the binary stay static musl (`FROM scratch`, no runtime shell/git/external proprietary files) within `image-budget.txt`?

When presenting an architectural recommendation to the user or in an ADR/PR, structure it concisely as:
- **Decision & Crate Placement:** What changes where (`schlange-config` / `schlange-core` / `schlange-github` / `ops`).
- **Invariants & Threat-Model Impact:** How `I0–I5`, Arztagent parity limits, and write-agent defences are preserved.
- **Test-First & Verification Plan:** Exact `sim` scenario, `schlange-github-fake` test, `sandbox-e2e` scenario (if GitHub-observable), and `verify-schlange` / `critic.sh` target.
