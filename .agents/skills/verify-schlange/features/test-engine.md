---
id: test-engine
title: "Test Engine (flaky detection, quarantine)"
priority: P3
arztagent: false
status: missing
inventory:
  - "api:*/ci/{owner}/repositories/*"
  - "cli:mergify tests *"
  - "cli:mergify ci junit-process"
  - "cli:mergify ci junit-upload"
problems:
  - "MQ-22"
source: "https://docs.mergify.com/test-engine"
---

# Test Engine (flaky detection, quarantine)

Collect JUnit results, detect flaky tests and quarantine them. A separate product.

## Sub-features

- `upload`
- `flaky`
- `quarantine`

## How to get to it (user POV)

- CLI in a CI job.

## Driving it

- Not planned.

## Gotchas

- –
