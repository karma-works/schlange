---
id: depends-on
title: "Dependencies between pull requests (`Depends-On`)"
priority: P1
arztagent: true
status: missing
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

- `parse`: lines `Depends-On: #<n>` in the description, also several; `owner/repo#<n>` and pull request URLs for other repositories.
- `limits`: the documented maximum number of dependencies; unresolvable references and owner restrictions are reported, not ignored.
- `cycles`: mutual dependencies never merge; report them.
- `hold`: the pull request is not queued while a named pull request is open.
- `release`: once the last named pull request is merged, the pull request is evaluated again.
- `closed-dependency`: a named pull request closed without merge keeps holding (report it).

## How to get to it (user POV)

- An agent writes `Depends-On: #123` in the description; the pull request waits with a visible
  reason, then enters the queue after #123 is merged.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: two pull requests in the simulated forge, the second with

## Gotchas

- In Mergify this is a merge protection, reported as its own check; the exact reporting must be
  observed before claiming conformance.
- Editing the description changes the dependency; evaluation follows the current description.
