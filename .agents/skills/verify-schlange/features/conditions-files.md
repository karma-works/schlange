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

## Driving it

- `schlange sim run scenarios/p0-queue-conditions.yaml` (a pull request touching a constitution file is not queued automatically).

## Gotchas

- A new push changes the file list; evaluation is bound to the head SHA (MQ-07).
