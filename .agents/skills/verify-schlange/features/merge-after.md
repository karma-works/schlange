---
id: merge-after
title: "Built-in protection `Merge-After`"
priority: P2
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-46"
  - "W-13"
source: "https://docs.mergify.com/merge-protections/builtin"
---

# Built-in protection `Merge-After`

A line `Merge-After: <timestamp>` in the pull request description holds the merge until that
time. A value that cannot be read as a timestamp blocks the pull request until the line is fixed,
and the protection's check says the date is invalid. It applies automatically to every pull
request that carries the line; Arztagent's rules do not use it.

## Sub-features

- `parse`: built (`crates/schlange-core/src/time.rs`). The documented forms: a date, a date and
  time, fractions of a second, `Z`, numeric offsets, no zone (UTC), and a bracketed zone that is
  UTC (`[UTC]`, `[Etc/UTC]`, `[GMT]`). With several lines the latest time counts (our choice).
- `hold`: built. Not queued before the time; the engine asks for a wake-up then (hold
  `merge-after` in the state). The simulation's time 0 is 2026-01-01T00:00:00Z.
- `invalid`: built. An unreadable value holds the pull request (a failing hold), it never fails
  open.
- `iana-zones`: missing. A named zone such as `[Europe/Paris]` needs the time zone database,
  which Schlange does not ship: it holds the pull request with a reason saying so (never merges
  early).
- `report`: the protection's check run is not posted; the reason is in the engine state.

## How to get to it (user POV)

- An author adds `Merge-After: 2030-09-01T09:00:00Z` to the description; the pull request waits with a visible reason.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-merge-after.yaml` → #1 is queued at `+1:00:00`, #2 (invalid) and #3 (`[Europe/Paris]`) are held with reasons, #4 (past) lands at once.
- **Unit tests.** `cargo test -p schlange-core time protections`.

## Gotchas

- Time zones: the timestamp carries its own offset; compare in UTC.
