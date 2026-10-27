---
id: config-file
title: "Find and load the configuration file"
priority: P1
arztagent: true
status: partial
inventory: []
problems:
  - "MQ-19"
  - "MQ-52"
source: "https://docs.mergify.com/configuration/file-format"
---

# Find and load the configuration file

Schlange reads the merge queue configuration from the repository, from the default branch only.
Several file names are allowed and are searched in a fixed order; two of them at once is an error.
A configuration change inside a pull request takes effect only once it is merged.

## Sub-features

- `search-order`: built (`schlange_config::config_file::NAMES`): `.mergify.yml`, `.mergify.yaml`,
  `.mergify/config.yml`, `.mergify/config.yaml`, `.github/mergify.yml`, `.github/mergify.yaml`.
- `default-branch-only`: built. `serve` reads the default branch at every reconciliation; the
  simulation (scenario option `config_in_repo`) after every step. A pull request's own copy never
  evaluates it.
- `two-files`: built. More than one file is an error (status `config_error`, the log); the last
  good configuration stays in use. In a pull request it fails the configuration check.
- `re-evaluate`: built. A changed configuration is loaded and `ConfigurationChanged` makes the
  engine evaluate every pull request again (attribute blocks and reported errors are dropped).
  On GitHub the change is seen at the next reconciliation (or a `reconcile_now`), not at the
  push webhook.
- `invalid`: a configuration that does not load keeps the last good one and is reported.
- `extends`: missing (feature `config-extends`).

## How to get to it (user POV)

- A human changes `.mergify.yml` through a pull request (class "constitution" in Arztagent).
- After the merge the new configuration applies to every pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p1-config-changed.yaml` → #2 (which drops `-label = halt` and carries `halt`) waits until the label goes (MQ-52); after it lands, `config  the configuration on the default branch changed; using it` and #3 (labelled `halt`) is queued (MQ-19).
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands a_changed_configuration_is_checked_and_applies_only_once_merged two_configuration_files_are_an_error_and_the_last_good_one_stays`.
- **Unit tests.** `cargo test -p schlange-config config_file`.

## Gotchas

- A pull request that loosens the configuration must not queue itself with it (MQ-52).
- Arztagent uses `.mergify.yml` at the root; the other names must still be recognised so that two files are caught as an error.
