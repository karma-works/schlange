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
- `operators`: `=`/`:`, `!=`/`≠`, `~=`, `*=`, `>=`, `>`, `<=`, `<` (parser built).
- `combinators`: `and`, `or`, `not` (parser and evaluation for the simulation built).
- `lists`: "any element", `!=` means "no element" (evaluation built).
- `value-types`: durations, schedules, timestamps as values (missing).

## How to get to it (user POV)

- Conditions in `queue_conditions`, `merge_conditions`, `conditions`; `schlange config validate` shows errors with their line.

## Driving it

- `cargo test -p schlange-config conditions` (parser), `cargo test -p schlange-core` (evaluation).
- `schlange sim run scenarios/p0-queue-conditions.yaml`

## Gotchas

- Regex with Python semantics (lookahead) through `fancy-regex`; a difference test against Python is still missing.
- Flapping conditions must not lead to duplicate entries (MQ-12).
