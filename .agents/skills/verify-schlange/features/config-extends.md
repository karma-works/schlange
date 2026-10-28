---
id: config-extends
title: "Inherit a configuration (`extends`)"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:extends"
problems:
  - "MQ-52"
source: "https://docs.mergify.com/configuration/sharing"
---

# Inherit a configuration (`extends`)

A configuration inherits another repository's configuration of the same owner, one level deep (`crates/schlange-config/src/extends.rs`).

## Sub-features

- `one-level`: built. The extended configuration is loaded first and merged: `shared` and `scopes` are never taken; `defaults`, `commands_restrictions`, `merge_queue` and `merge_protections_settings` merge key by key (local wins); `pull_request_rules`, `queue_rules`, `priority_rules` and `merge_protections` merge by name (extended rules first, a local rule of the same name replaces it). An extended configuration with `extends` and a configuration extending its own repository are errors.
- `same-org`: built. `serve` reads the other repository's configuration file (every documented name) on its default branch, at every configuration load, so a change there applies at the next reconciliation.
- `visibility`: not built: the rule that the source must be at least as visible, and the rule about pull requests that point `extends` elsewhere, are not checked.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-config-extends.yaml` (the scenario's `extends` maps a repository name to a file).
- **Unit.** `cargo test -p schlange-config extends`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP). (`extends_reads_the_other_repository`).

## Gotchas

- `serve` downscopes its token to the served repositories: the extended repository must be one of them (`--repo`) or GitHub refuses the read.
- `schlange config validate` checks the local file only.
