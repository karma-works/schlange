---
id: action-backport
title: "Action `backport`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:backport"
  - "config:pull_request_rules.actions.backport"
  - "config:pull_request_rules.actions.backport.*"
  - "config:defaults.actions.backport"
  - "config:defaults.actions.backport.*"
  - "default:pull_request_rules.actions.backport.*"
  - "default:defaults.actions.backport.*"
  - "enum:merge_conflict_style:*"
  - "enum:ReportMode:*"
unsupported:
  - "config:pull_request_rules.actions.backport.bot_account"
  - "config:defaults.actions.backport.bot_account"
  - "config:pull_request_rules.actions.backport.merge_conflict_style"
  - "config:defaults.actions.backport.merge_conflict_style"
  - "enum:merge_conflict_style:diff3"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/backport"
---

# Action `backport`

After the merge, copies the pull request onto other branches as new pull requests. Arztagent does not use the action.

## Sub-features

- `run`: built. Edge-triggered (docs: workflow, "How rules are evaluated"): runs once when the rule starts to match, again only after it stopped matching. A rule that matches while the pull request is open waits for the merge; the merge commit (the queue's squash commit) is cherry-picked onto each destination as one commit, on the branch `mergify/bp/<branch>/pr-<n>`, with a new pull request.
- `options`: built: `branches`, `regexes` (anchored at the start, against the repository's branches), `labels`, `assignees` (`{{ author }}`, `{{ merged_by }}`; `[bot]` logins skipped), `title` and `body` (with `destination_branch`, `cherry_pick_error`, `cherry_picked_commits`), `ignore_conflicts` with `label_conflicts`, `report_mode` (`check`: `Rule: <rule> (backport)`, `comment`). Defaults for title and body are Schlange's own words.
- `conflicts`: in the simulated forge a conflicting copy is created with conflict markers and the conflict label; on GitHub the API cannot commit conflict markers, so a conflicting copy fails and is reported.
- `command`: `@mergifyio backport <branches>` built; on an open pull request it waits for the merge.
- `idempotence`: a copy whose branch exists reuses its pull request; after a crash the copy is not made twice.
- `merge_conflict_style`, `bot_account`: not built.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-copies.yaml`.
- **Engine.** `cargo test -p schlange-core --test actions`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Crashes.** `cargo test -p schlange-forge --test action_crashes`: a crash before and after every side effect of the action scenarios posts nothing twice and leaves the same labels, assignees, checks and copies.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- `bot_account` is accepted and ignored: Schlange acts as its app (impersonating users needs their tokens); the support matrix flags it.
- Each copy is a single cherry-picked commit, not one commit per original commit.
- Mergify's limit of 1000 branches for `regexes` is not enforced.
