---
id: S-AUDIT
title: "Evidence per merge"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-02"
  - "MQ-31"
source: "KONZEPT.md, section 8"
---

# Evidence per merge

For every merge an evidence record: tested SHA and tree, base, pull request heads, landed SHA;
available through CLI and MCP.

## Sub-features

- `record`: built in `schlange serve`. One row per landed pull request in the SQLite table `audit` (`crates/schlange-github/src/store.rs`): repository, time, car, pull request, the tested head, the tree the car tested for this pull request (its prefix tree), the expected base, the landed commit, its tree and its actual parent, and a note for anything unusual (a landing found merged after a lost reply, a parent that is not the expected base). The same line goes to the log (`audit: #n car c tested … landed …`). Written for landings found after a crash too.
- `query`: missing. No `schlange audit <pr>` and no MCP tool yet; the table can be read with any SQLite client (`sqlite3 /data/schlange.db 'select * from audit'`).
- In the simulation the event log is the evidence (`sim run --json`).

## How to get to it (user POV)

- `schlange audit <pr>` (planned). Today: the `audit` table in `--data-dir`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Fake GitHub.** `cargo test -p schlange-github-fake --test serve a_crash_mid_landing_recovers_the_queue_and_never_lands_twice`: after a crash right after a landing and the restart, the audit has exactly one row per landed pull request, each with the tested tree equal to the landed tree and the expected base equal to the actual parent.

## Gotchas

- The pull request heads of the other pull requests in a batch are in their own rows (one row per pull request), not repeated in each.
