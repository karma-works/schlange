---
id: config-simulator
title: "Configuration simulator"
priority: P2
arztagent: false
status: partial
inventory:
  - "api:POST /repos/{owner}/{repository}/configuration-simulator"
  - "api:POST /repos/{owner}/{repository}/pulls/{number}/simulator"
  - "api:POST /repos/{owner}/{repository}/simulator"
  - "cli:mergify config simulate"
unsupported:
  - "api:POST /repos/{owner}/{repository}/configuration-simulator"
  - "api:POST /repos/{owner}/{repository}/simulator"
problems:
  - "W-12"
source: "https://docs.mergify.com/configuration/file-format"
---

# Configuration simulator

Evaluate a configuration against real pull requests without acting. In Schlange this is
`S-SIMULATE`.

## Sub-features

- `against-pr`: built (`POST /repos/{owner}/{repo}/pulls/{n}/simulator` of `serve`, and
  `schlange config simulate <file> --pr <n>`); see `S-SIMULATE.md`.
- `against-repo`: missing (`POST …/simulator` and `POST …/configuration-simulator` without a
  pull request).

## How to get to it (user POV)

- `schlange config simulate <file> --pr <n>`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange config simulate crates/schlange-config/tests/golden/arztagent.yml --pr 1 --scenario scenarios/p0-queue-conditions.yaml`. Exit `0`.

## Gotchas

- The answer's `summary` is Schlange's explanation text, not Mergify's check-run text.
