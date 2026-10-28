---
id: S-SIMULATE
title: "`schlange config simulate`"
priority: P2
arztagent: false
status: partial
inventory: []
problems:
  - "W-12"
source: "KONZEPT.md, section 8"
---

# `schlange config simulate`

Evaluates a new configuration against a real pull request without acting: is it valid, would
the pull request be queued and under which rule, which conditions fail.

## Sub-features

- `against-pr`: built. The configuration is validated like `config validate` (stages 1–4;
  stage 5 findings come back as `warnings`), then the pull request as the engine knows it is
  explained under the new rules (`schlange_core::explain`). The answer has the public
  simulator's `title`, `summary` and `deprecations` (always empty), plus the explanation.
  Against a running `serve` (`POST /repos/{owner}/{repo}/pulls/{n}/simulator`, body
  `{"mergify_yml": …}`, admin token), or against the end of a scenario (`--scenario`).
- `against-repo`: missing (see `config-simulator`).
- What it does not simulate: the queue's future (batches, landing order) under the new rules;
  holds and waiting commands are the engine's current ones.

## How to get to it (user POV)

- `schlange config simulate <file> --pr <n> --repo owner/name [--url …] [--json]`
- `schlange config simulate <file> --pr <n> --scenario scenarios/<file>.yaml`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange config simulate crates/schlange-config/tests/golden/arztagent.yml --pr 1 --scenario scenarios/p0-queue-conditions.yaml`. Exit `0`; a title and the explanation of #1.
- **Against serve.** `cargo test -p schlange --test serve`: a valid configuration answers with the explanation (exit 0), an invalid one exits 1.

## Gotchas

- Needs the Mergify schema (`.research/mergify/schema.json` or `--schema`).
