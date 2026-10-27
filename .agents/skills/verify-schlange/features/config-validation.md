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
- `stage-semantics`: unique names, references to existing queues (missing).
- `stage-support`: every used path, attribute, action and enum value against the support matrix built from the feature files (built).
- `json`: `--json` gives the same findings machine-readable (built).

## How to get to it (user POV)

- `schlange config validate .mergify.yml` before a commit or in CI.
- Without a schema (`--schema` or `.research/mergify/schema.json`) the command stops with a clear message (exit 3).

## Driving it

- `schlange config validate crates/schlange-config/tests/golden/arztagent.yml` → exit 0.
- Golden files: `crates/schlange-config/tests/golden/*.yml` with expected output `*.txt` and `*.json`; `cargo test -p schlange-config --test golden`.

## Gotchas

- Mergify parses YAML with Python (YAML 1.1); `on`, `yes` and octal numbers behave differently than in YAML 1.2. A difference test is still missing.
- The semantics stage is missing; duplicate rule names are not reported.
- Whether the schema may be embedded in the binary is open (question for the human).
