---
id: config-shared
title: "Shared anchors (`shared`)"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:shared"
problems:
  - "MQ-19"
source: "https://docs.mergify.com/configuration/file-format"
---

# Shared anchors (`shared`)

A free section for YAML anchors that are reused elsewhere through aliases.

## Sub-features

- `anchors`

## How to get to it (user POV)

- Key `shared`.

## Driving it

- `schlange config validate` resolves anchors and aliases (YAML stage).

## Gotchas

- Error positions inside aliases point at the use.
