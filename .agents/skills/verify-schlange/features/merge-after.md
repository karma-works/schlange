---
id: merge-after
title: "Built-in protection `Merge-After`"
priority: P2
arztagent: false
status: missing
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

- `parse`: timestamp in the documented format (see data types).
- `hold`: no merge before the time; then evaluated again (virtual clock in the simulation).
- `invalid`: unreadable value blocks and is reported, it does not fail open.

## How to get to it (user POV)

- An author adds `Merge-After: 2030-09-01T09:00:00Z` to the description; the pull request waits with a visible reason.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned scenario: a pull request with `Merge-After` one hour ahead on the virtual clock, expected merge only after that hour; one with an invalid value, expected block with reason.

## Gotchas

- Time zones: the timestamp carries its own offset; compare in UTC.
