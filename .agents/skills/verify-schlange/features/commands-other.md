---
id: commands-other
title: "Commands `backport`, `copy`, `rebase`, `squash`, `update`"
priority: P2
arztagent: false
status: partial
inventory:
  - "cmd:backport"
  - "cmd:copy"
  - "cmd:rebase"
  - "cmd:squash"
  - "cmd:update"
problems:
  - "MQ-50"
source: "https://docs.mergify.com/commands"
---

# Commands `backport`, `copy`, `rebase`, `squash`, `update`

Further comment commands that run the operations of the actions of the same name once
(`crates/schlange-core/src/actions.rs`, `command`). Every command is checked against its
restriction first (documented defaults included, see `commands-restrictions`), acknowledged
with 👍, and answered in a reply once the forge answered. Like `queue`, an edited comment never
runs, and a command acts only on the head it vouched for (the head GitHub's activity log shows
at the comment): if the pull request moved on, nothing is done and the reply says so; a waiting
`backport` is cancelled when the pull request merges at another head. `update`, `rebase` and
`squash` are never resent after a failure or a restart (the reply asks to comment again).

## Sub-features

- `update`: built. Merges the base in; answers when the pull request is up to date, closed or
  queued (the queue tests it on its base anyway).
- `rebase`: built in the simulated forge; on GitHub the adapter refuses (no commit replay through
  the API) and says to use `update`. Refused for forks.
- `squash [all-commits|first-commit|title+body]`: built; answers when there is one commit.
- `copy <branches>`: built; a new pull request per branch (`mergify/copy/<branch>/pr-<n>`).
- `backport <branches>`: built; on a merged pull request at once, on an open one after the merge
  (cancelled with a reply when it is closed unmerged). Without branches the command is answered
  with an error (our choice: the docs do not say which branches a bare `backport` uses).

## How to get to it (user POV)

- Comment on the pull request, e.g. `@mergifyio backport stable`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-copies.yaml`: `copy`, a
  waiting `backport` and `update` by command; `scenarios/p1-commands.yaml`: `rebase` on an
  up-to-date pull request is answered.
- **Engine.** `cargo test -p schlange-core --test actions backport_and_squash_commands` and
  `--test commands action_commands_run_or_explain`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (`copy`, `squash`)
  and `--test e2e_commands`.

## Gotchas

- Permissions, see `commands-restrictions`.
- `bot_account` of the actions does not apply to commands either: Schlange acts as its app.
