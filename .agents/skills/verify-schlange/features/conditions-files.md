---
id: conditions-files
title: "Attribute `files` with regex"
priority: P1
arztagent: true
status: missing
inventory:
  - "attr:files"
problems:
  - "MQ-07"
  - "MQ-52"
source: "https://docs.mergify.com/configuration/conditions"
---

# Attribute `files` with regex

`files` is the list of paths changed by the pull request. Arztagent excludes constitution paths
with a negated regex with lookahead, which needs Python-compatible regex semantics.

## Sub-features

- `list`: all changed paths, renamed ones too.
- `regex`: search like Python's `re.search`, lookahead through `fancy-regex`.
- `negation`: `-files ~= …` means "no file matches".

## How to get to it (user POV)

- A pull request changes `.github/workflows/ci.yml` → the rule does not apply, a human must queue it.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; `#2` (touches `.github/workflows/ci.yml`) is not queued, `#9` (exception path under `.agents/skills/verify-arztagent/features/`) is queued.

## Gotchas

- A new push changes the file list; evaluation is bound to the head SHA (MQ-07).
