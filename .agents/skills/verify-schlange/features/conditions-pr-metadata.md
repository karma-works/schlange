---
id: conditions-pr-metadata
title: "Further pull request metadata"
priority: P2
arztagent: false
status: partial
inventory:
  - "attr:merged"
  - "attr:closed"
  - "attr:locked"
  - "attr:linear-history"
  - "attr:number"
  - "attr:merged-by"
  - "attr:merge-commit-sha"
  - "attr:milestone"
  - "attr:head"
  - "attr:head-repo-full-name"
  - "attr:title"
  - "attr:body"
  - "attr:body-raw"
  - "attr:repository-name"
  - "attr:repository-full-name"
  - "attr:assignee"
  - "attr:commits-unverified"
  - "attr:co-authors"
  - "attr:commits-behind"
  - "attr:commits"
  - "attr:updates"
  - "attr:updated-at"
  - "attr:created-at"
  - "attr:closed-at"
  - "attr:merged-at"
unsupported:
  - "attr:merged"
  - "attr:locked"
  - "attr:linear-history"
  - "attr:merged-by"
  - "attr:merge-commit-sha"
  - "attr:milestone"
  - "attr:head-repo-full-name"
  - "attr:body"
  - "attr:body-raw"
  - "attr:repository-name"
  - "attr:repository-full-name"
  - "attr:assignee"
  - "attr:commits-unverified"
  - "attr:co-authors"
  - "attr:commits-behind"
  - "attr:commits"
  - "attr:updates"
  - "attr:updated-at"
  - "attr:created-at"
  - "attr:closed-at"
  - "attr:merged-at"
problems:
  - "MQ-09"
source: "https://docs.mergify.com/configuration/conditions"
---

# Further pull request metadata

State, texts, commits, timestamps and origin of a pull request as conditions.

## Sub-features

- `state`: `closed` built in the evaluator; `merged`, `locked`, `linear-history` missing.
- `identity`: `number`, `head` (branch name) and `title` built; `merged-by`, `merge-commit-sha`, `milestone`, `assignee`, `head-repo-full-name`, `repository-name`, `repository-full-name` missing.
- `texts`: `body`, `body-raw` missing.
- `commits`: `commits`, `commits-behind`, `commits-unverified`, `co-authors`, `updates` missing.
- `timestamps`: `created-at`, `updated-at`, `closed-at`, `merged-at` missing.

## How to get to it (user POV)

- Conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet** as a scenario of its own. The built attributes are covered by `cargo test -p schlange-core` (evaluator tests); `target/release/schlange config validate` flags every missing attribute (feature `conditions-pr-metadata`, `not built in this partial feature`).

## Gotchas

- `commits-behind` only with the length operator.
