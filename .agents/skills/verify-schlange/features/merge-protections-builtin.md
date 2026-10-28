---
id: merge-protections-builtin
title: "Built-in protection: configuration changed"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:mergify-configuration-changed"
problems:
  - "MQ-19"
  - "MQ-52"
source: "https://docs.mergify.com/merge-protections/builtin"
---

# Built-in protection: configuration changed

When a pull request against the default branch changes the merge queue configuration file, the
new configuration is validated and the result is reported as a check of its own. That check must
succeed before the pull request can merge, so a configuration that does not validate never
lands. Deleting the file is reported as a success under a separate check. Pull requests that do
not touch the file are unaffected. It applies automatically, so it is in effect for Arztagent,
whose constitution pull requests change `.mergify.yml`.

## Sub-features

- `detect`: built (`schlange_config::config_file`). The pull request targets the default branch
  and changes one of the six configuration file names.
- `validate`: built. The configuration as the pull request leaves it: one file (two or more are
  a failure); with the schema at hand (`SCHLANGE_SCHEMA` or `.research/`) stages 1 to 5 (findings
  Schlange does not support are notes, the file is valid for Mergify), without it stages 1, 3
  and 4 (YAML, shape and conditions, semantics). The simulation always runs without the schema.
- `report`: built. A check run `Configuration changed` (success or failure, findings with lines
  in the summary) on the head; a failure holds the pull request (hold
  `configuration-changed`).
- `deleted`: built. Removing the configuration is reported as a success under `Configuration
  has been deleted`.
- `attribute`: built. `mergify-configuration-changed` is true when the files include a
  configuration name.
- `not-active`: built. The new configuration takes effect only once it is on the default branch
  (see `config-file`).

## How to get to it (user POV)

- A human opens a pull request that edits `.mergify.yml`; the check appears with the validator's
  findings and positions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Validator part.** `target/release/schlange config validate <changed file>`.
- **Scenario.** `target/release/schlange sim run scenarios/p1-config-changed.yaml` → `check `Configuration changed` on #1: failure` (duplicate queue names), `#4` (a second file) fails, `#2` succeeds and lands.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands a_changed_configuration_is_checked_and_applies_only_once_merged` (check runs of the app on the heads).

## Gotchas

- Exact check names come from the docs; their texts are Schlange's own. Observe Mergify's output
  in the sandbox before claiming conformance.
- The new configuration only takes effect after the merge (MQ-19); the check validates, it does not activate.
- The deletion check is covered by unit tests only (the simulated forge and the fake have no
  file deletion in a pull request).
