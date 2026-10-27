---
id: queue-commit-message
title: "Commit message on merge"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.commit_message_format"
  - "config:queue_rules.commit_message_format.*"
  - "config:queue_rules.commit_message_template"
  - "default:queue_rules.commit_message_format.*"
  - "enum:title:*"
  - "enum:body:*"
  - "enum:trailers:*"
problems:
  - "MQ-20"
source: "https://docs.mergify.com/merge-queue/merge-strategies"
---

# Commit message on merge

A squash produces the commit message in a fixed format. By default it inherits the forge's
behaviour (title and body as GitHub builds them on squash). Title, body and trailers can be
chosen.

## Sub-features

- `default-inherit`: title and body as the forge does it.
- `title`, `body`: pull request title or body, or empty.
- `trailers`: co-authors, approvals, merger.
- `template`: old template field (deprecated).

## How to get to it (user POV)

- Look at the commit on `main` after the merge.

## Driving it

- Golden files per method (unit) once merging is built.

## Gotchas

- "As the forge does it" depends on the repository's squash message setting; test both variants.
