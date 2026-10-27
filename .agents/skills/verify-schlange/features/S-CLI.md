---
id: S-CLI
title: "Complete CLI with `--json`"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-43"
  - "W-25"
source: "KONZEPT.md, section 8"
---

# Complete CLI with `--json`

Every function is reachable through the CLI, every output exists with `--json`, exit codes are
stable and there are no interactive prompts.

## Sub-features

- `config-validate`: built.
- `sim-run`: built.
- `doctor`: built (minimal).
- `queue`, `pr explain`, `events`: missing.

## How to get to it (user POV)

- `schlange --help`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Help.** Run `target/release/schlange --help`. Lists `config`, `sim`, `support-matrix`, `doctor`.
- **JSON everywhere.** Run each command with `--json`; stdout parses as JSON (`crates/schlange/tests/cli.rs`).
- **Exit codes.** As in `AGENTS.md`: see the bullets of `config-validation`.

## Gotchas

- Exit codes are documented in `AGENTS.md`.
