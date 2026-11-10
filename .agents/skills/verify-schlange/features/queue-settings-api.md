---
id: queue-settings-api
title: "Merge queue settings through the API"
priority: P2
arztagent: false
status: missing
inventory:
  - "api:* /orgs/{owner}/merge-queue/settings"
  - "api:* /repos/{owner}/{repository}/merge-queue/settings"
  - "api:* /repos/{owner}/{repository}/merge-queue/settings/branches*"
  - "api:GET /repos/{owner}/{repository}/branches/{branch}/merge-queue/settings"
problems:
  - "W-10"
  - "W-11"
  - "W-18"
source: "https://docs.mergify.com/api-usage"
---

# Merge queue settings through the API

Some merge queue settings are kept outside the configuration file and set through the API, on
three levels: the organization, the repository and a base branch name or pattern. A lower level
overrides a higher one, and an unset value falls through to the next level up. In the sources
pinned on 2026-11-10 the only such setting is the batch pull request switch, which was
`merge_queue.batch_pull_request` in the configuration file before.

## Sub-features

- `organization`: missing. Read, replace and reset the settings of an organization.
- `repository`: missing. Read, replace and reset the settings of a repository.
- `branches`: missing. List, read, replace and reset the settings of a base branch name or pattern.
- `effective`: missing. The resolved settings of a base branch, with the level each value comes from.
- `batch-pull-request`: missing. The only setting so far; Schlange has no switch and always opens one draft per batch ([queue-draft-pr](queue-draft-pr.md)).

## How to get to it (user POV)

- None in Schlange. Schlange's configuration is the `.mergify.yml` of the default branch; it keeps no settings of its own.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: decide whether Schlange needs settings outside the configuration file at all; Arztagent sets none.

## Gotchas

- Settings kept in the API are invisible in `.mergify.yml`: a repository can behave differently from what its configuration file says (W-11). Moving to Schlange loses them silently unless they are read out first.
- `config validate` cannot see these settings, so it cannot warn about them.
