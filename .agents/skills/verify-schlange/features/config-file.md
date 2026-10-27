---
id: config-file
title: "Find and load the configuration file"
priority: P1
arztagent: true
status: missing
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

- `search-order`: the allowed file names are searched in a fixed order, the first hit wins.
- `default-branch-only`: the configuration in a pull request's head never evaluates that pull request.
- `two-files`: more than one configuration file is a configuration error.
- `re-evaluate`: after a configuration change is merged, every open pull request is evaluated again.

## How to get to it (user POV)

- A human changes `.mergify.yml` through a pull request (class "constitution" in Arztagent).
- After the merge the new configuration applies to every pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **CLI.** Locally: `schlange config validate <file>` checks one file; the search in a repository is not built yet.
- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- A pull request that loosens the configuration must not queue itself with it (MQ-52).
- Arztagent uses `.mergify.yml` at the root; the other names must still be recognised so that two files are caught as an error.
