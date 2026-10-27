---
id: conditions-syntax
title: "Condition syntax and combinators"
priority: P1
arztagent: true
status: partial
inventory: []
problems:
  - "MQ-11"
  - "MQ-12"
source: "https://docs.mergify.com/configuration/conditions"
---

# Condition syntax and combinators

A condition is a string of the form `[-][#]attribute [operator value]`, plus the combinators
`and`, `or` and `not` as lists. For list attributes one matching element is enough, except for
`!=`. Arztagent uses `=`, `~=`, negation, length (`#check-failure = 0`) and `or`.

## Sub-features

- `parser`: negation, length, attribute, operator, value (built, `schlange-config`).
- `operators`: `=`/`:`, `!=`/`≠`, `~=`, `*=`, `>=`/`≥`, `>`, `<=`/`≤`, `<` (parser built).
- `combinators`: `and`, `or`, `not` (parser and evaluation built). Three-valued: an atom that cannot be evaluated (unknown attribute, bad value, regex error) is unknown; `and`, `or` and `not` keep a result only when it does not depend on the unknown part, and an unknown result counts as false, so an error fails closed also under `not` (critic 1abc).
- `author-template`: built. `{{ author }}` as a value stands for the pull request's author; numeric, time and permission attributes reject it as an invalid value.
- `yaml-1.1`: built. Plain `yes`/`no`/`on`/`off` (any case form PyYAML knows) are booleans, as Mergify's loader reads YAML; quoted they stay strings. The docs do not say this; it follows from Mergify loading its configuration with a YAML 1.1 loader (our assumption, to be confirmed by a difference test).
- `lists`: "any element", `!=` means "no element" (evaluation built).
- `value-types`: durations, schedules, timestamps as values (missing).

## How to get to it (user POV)

- Conditions in `queue_conditions`, `merge_conditions`, `conditions`; `schlange config validate` shows errors with their line.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Tests.** `cargo test -p schlange-config conditions` (parser), `cargo test -p schlange-core` (evaluation).
- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; the `or` with `author = dependabot[bot]` queues `#3` without `KI-Review`, `#4` (major update) stays out.

## Gotchas

- Regex with Python semantics (lookahead) through `fancy-regex`; a difference test against Python is still missing.
- Flapping conditions must not lead to duplicate entries (MQ-12).
