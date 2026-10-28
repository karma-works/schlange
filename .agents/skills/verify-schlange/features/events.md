---
id: events
title: "Event log"
priority: P2
arztagent: false
status: partial
inventory:
  - "api:GET /repos/{owner}/{repository}/logs"
  - "api:GET /repos/{owner}/{repository}/logs/aggregate"
  - "cli:mergify events"
unsupported:
  - "api:GET /repos/{owner}/{repository}/logs/aggregate"
problems:
  - "MQ-43"
  - "W-03"
source: "https://docs.mergify.com/merge-queue/monitoring"
---

# Event log

Every decision of the queue and every operator control as a log that can be queried.

## Sub-features

- `list`: built (`crates/schlange-github/src/events.rs`). Every engine decision except reads
  and timers, and every pause and freeze, as a record shaped like the public event log
  (`id`, `received_at`, `type`, `pull_request`, `base_ref`, `trigger`, `metadata`); types
  `action.queue.enter`, `action.queue.leave`, `action.queue.checks_start`,
  `action.queue.checks_end`, `action.queue.merged`, `queue.pause.create|delete`,
  `scheduled_freeze.create|delete`, and `schlange.<kind>` for decisions without a documented
  type (landing, halt, car closed, cosmetics, new actions). Persisted in SQLite (`events`)
  with a data directory, in memory otherwise. Filters: pull request (also as part of a batch),
  `received_from`, `event_type`, `per_page` with a `cursor` (`next_cursor`).
- `aggregate`: missing (`logs/aggregate`).
- In the simulation the event log of `sim run` is the log.

## How to get to it (user POV)

- `schlange events [--pr N] [--since <RFC 3339|unix>] [--limit N] [--json] --repo owner/name [--url …|--data-dir …]`
- `GET /repos/{owner}/{repo}/logs` (admin token)

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Against serve.** `cargo test -p schlange --test serve`: after a landing, `schlange events --pr <n> --json` lists `action.queue.enter`, `action.queue.checks_start`, `action.queue.merged`; `schlange events` lists `queue.pause.create`.
- **Store.** `cargo test -p schlange-github --lib events_filter_by_pull_request_time_and_page`.
- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; the `event log` section lists every event and decision with its virtual time.

## Gotchas

- The metadata is Schlange's own, not Mergify's; event types without a documented counterpart
  start with `schlange.`.
- In shadow mode the trigger says the decision was not carried out.
