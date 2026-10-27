---
id: merge-protections-builtin
title: "Built-in protection: configuration changed"
priority: P1
arztagent: true
status: missing
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

- `detect`: the pull request changes one of the configuration file names (see `config-file`).
- `validate`: the new file goes through `schlange config validate` (stages 1 to 5).
- `report`: a check with the result; failure blocks the merge.
- `deleted`: removing the file is reported as success under its own check name.
- `attribute`: `mergify-configuration-changed` is true for such pull requests.

## How to get to it (user POV)

- A human opens a pull request that edits `.mergify.yml`; the check appears with the validator's
  findings and positions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Validator part.** Run `target/release/schlange config validate <changed file>`; this is the check's content (built).
- **Not drivable yet.** The check on the pull request needs the forge adapter; planned scenario: a pull request changing the configuration, expected check result and blocked merge on failure.

## Gotchas

- Exact check names and their texts are Mergify's; observe them in the sandbox before claiming conformance.
- The new configuration only takes effect after the merge (MQ-19); the check validates, it does not activate.
