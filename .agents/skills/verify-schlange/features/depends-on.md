---
id: depends-on
title: "Dependencies between pull requests (`Depends-On`)"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:depends-on"
problems:
  - "MQ-49"
  - "MQ-09"
  - "MQ-27"
source: "https://docs.mergify.com/merge-protections/builtin"
---

# Dependencies between pull requests (`Depends-On`)

A pull request names other pull requests in its description with a line `Depends-On: #<n>`. It
is held back until every named pull request is merged; the attribute `depends-on` lists them.
Arztagent uses no stacked pull requests: every pull request targets `main`, and a dependency on
another open pull request is written this way (Arztagent `AGENTS.md`, `PLAN-MERGIFY.md`). Not in
KONZEPT.md's P1 list; added after checking Arztagent's rules.

## Sub-features

- `parse`: built (`crates/schlange-core/src/protections.rs`). `Depends-On: #n`,
  `Depends-On: owner/repo#n`, pull request URLs; the key without regard to letter case, each
  reference once.
- `limits`: built. More than 50 references resolve none and fail with the count. A reference
  that is no reference, does not exist or cannot be read, or belongs to another owner fails
  (does not wait).
- `cycles`: built. A circular chain (or a self-reference) never resolves; the hold names the
  cycle (`#5 → #6 → #5`).
- `hold`: built. Not queued while a named pull request is open (a hold `depends-on` in the
  engine state, for `pr explain`); a queued pull request whose body gains an unmet dependency
  leaves with `PR_DEQUEUED` (code assumed).
- `release`: built. Once the last named pull request is merged, the next evaluation queues it
  (the engine's own view of a pull request it knows wins over the dependent's read).
- `closed-dependency`: built. Closed without merge fails the protection.
- `other-repositories`: GitHub reads them with the same installation; one it cannot read fails.
  The queue does not show dependencies in its own lists (no dashboard).
- `attribute`: built. `depends-on` lists `owner/repo#n`.
- `report`: Mergify reports the protection as a check; Schlange reports it in the engine state
  and the status, not as a check run (missing).

## How to get to it (user POV)

- An agent writes `Depends-On: #123` in the description; the pull request waits with a visible
  reason, then enters the queue after #123 is merged.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p1-depends-on.yaml` → merged order `[9, 1]`; the `held` section names the closed dependency, the cycle, the missing and the foreign reference.
- **Unit and engine tests.** `cargo test -p schlange-core protections`; `cargo test -p schlange-core --test commands depends_on_holds_until_the_dependency_merges`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands depends_on_and_merge_after_hold_with_visible_reasons`.

## Gotchas

- In Mergify this is a merge protection, reported as its own check; the exact reporting must be
  observed before claiming conformance.
- Editing the description changes the dependency; evaluation follows the current description.
