---
id: commands-restrictions
title: "Who may run commands (`commands_restrictions`)"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:commands_restrictions"
  - "config:commands_restrictions.*"
  - "default:commands_restrictions.*"
  - "attr:sender-permission"
  - "attr:from-fork"
  - "enum:GitHubRepositoryPermission:*"
problems:
  - "MQ-50"
  - "MQ-51"
source: "https://docs.mergify.com/commands/restrictions"
---

# Who may run commands (`commands_restrictions`)

Per command a list of conditions says who may trigger it. Without settings: write permission on
the repository; for some commands the author of a fork pull request as well. Arztagent restricts
`queue` to two humans.

## Sub-features

- `own-rule`: built (`schlange_config::rules::CommandsRestrictions`). Conditions per command,
  any attribute, `conditions: true` lets anyone run it.
- `defaults`: built. `sender-permission >= write` for every command; for `rebase`, `refresh`,
  `squash` and `update` also `sender = {{author}}` together with `from-fork`. A command without
  an entry in the schema gets the default of `queue`.
- `permission-ladder`: built. `sender-permission` compares on none < read < triage < write <
  maintain < admin; an unknown or unread permission counts as `none` (fail closed).
- `from-fork`: built. GitHub: head repository differs from the base repository, or the head
  repository was deleted.
- `check-time`: built. The adapter reads the sender's permission when it hands the comment to
  the engine (GitHub: `GET /collaborators/{user}/permission`; the simulation: the scenario's
  `permissions` table, unknown logins `none`); the engine evaluates the restriction when it
  runs the command. A comment whose permission cannot be read waits for the next round.
- `template`: built. `{{author}}` (also `{{ author }}`) in a restriction is replaced by the pull
  request's author.
- `checkbox`: built. A ticked checkbox is `queue` by whoever ticked it, under the same
  restriction.

## How to get to it (user POV)

- An agent comments `@mergifyio queue` → refusal with a reply.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Arztagent's real flow.** `target/release/schlange sim run scenarios/p1-arztagent-constitution.yaml`
  → `@arztagent-bot, you may not run `queue` on this pull request` (write permission is not
  enough), then `enqueue #1 into `default` (rule `queue command by @karma-works`)`.
- **Matrix in the engine.** `cargo test -p schlange-core --test commands restrictions_refuse_with_a_reply_and_defaults_need_write`
  (sender list, write default for `dequeue`, fork author for `refresh`); `cargo test -p schlange-core --lib eval` (ladder, template).
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands arztagent_constitution_pull_request_waits_for_a_listed_human`.

## Gotchas

- `sender-permission` needs the forge's permission API; a table in the simulator.
- GitHub returns `none` for a non-collaborator on a private repository and `read` on a public
  one; the fake returns `none`.
