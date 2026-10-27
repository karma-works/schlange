---
id: config-validation
title: "Validate a configuration"
priority: P1
arztagent: true
status: partial
inventory:
  - "cli:mergify config validate"
problems:
  - "MQ-19"
  - "W-05"
  - "W-12"
source: "https://docs.mergify.com/configuration/file-format"
---

# Validate a configuration

Schlange checks a configuration offline and reports every finding with path, line and column. The
check has five stages: YAML, Mergify schema, conditions, semantics and support by Schlange. Exit 0
means valid, 1 invalid, 2 valid for Mergify but not supported by Schlange (only with `--strict`).

## Sub-features

- `stage-yaml`: syntax, duplicate keys, anchors and aliases, position of every finding (built).
- `stage-schema`: types, enums, allowed keys and limits against the pinned schema (built).
- `stage-conditions`: attribute known, operator allowed for the attribute, regex compiles with Python semantics through `fancy-regex` (built).
- `stage-semantics`: unique rule names, `queue` action names an existing queue, durations readable, `checks_timeout` ≥ 60 s (built; only rules the docs state).
- `stage-support`: every used path, attribute, action and enum value against the support matrix built from the feature files (built).
- `json`: `--json` gives the same findings machine-readable (built).

## How to get to it (user POV)

- `schlange config validate .mergify.yml` before a commit or in CI.
- Without a schema (`--schema` or `.research/mergify/schema.json`) the command stops with a clear message (exit 3).

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.
- `.research/mergify/schema.json` present (`tools/fetch-mergify-sources`).

- **Valid configuration.** Validate Arztagent's file. Run `target/release/schlange config validate crates/schlange-config/tests/golden/arztagent.yml`. Exit `0`; the last line says `valid for Mergify` and lists unsupported features grouped by feature ID.
- **Schema error with position.** Run `target/release/schlange config validate crates/schlange-config/tests/golden/bad-schema.yml`. Exit `1`; stdout has `bad-schema.yml:3:19: error[schema] /queue_rules/0/merge_method`.
- **Semantic errors.** Run `target/release/schlange config validate crates/schlange-config/tests/golden/bad-semantics.yml`. Exit `1`; `error[semantics]` for duplicate names, an unknown queue, an unreadable duration.
- **Condition errors.** Run `target/release/schlange config validate crates/schlange-config/tests/golden/bad-conditions.yml`. Exit `1`; one `error[conditions]` line per broken condition (unknown attribute, operator, regex).
- **Strict support check.** Run `target/release/schlange config validate --strict crates/schlange-config/tests/golden/unsupported.yml`. Exit `2`.
- **No schema.** Run `target/release/schlange config validate --schema /nonexistent.json crates/schlange-config/tests/golden/arztagent.yml`. Exit `3` with a message naming `tools/fetch-mergify-sources`.
- **Proof.** `--json` of the same calls; `findings[].line` and `column` are set. Golden files: `cargo test -p schlange-config --test golden`.

## Gotchas

- Mergify parses YAML with Python (YAML 1.1); `on`, `yes` and octal numbers behave differently than in YAML 1.2. A difference test is still missing.
- The semantics stage checks only rules the documentation states; runtime incompatibilities (rulesets, forks) are reported by the queue, not the validator.
- Whether the schema may be embedded in the binary is open (question for the human).
