---
id: commands-restrictions
title: "Who may run commands (`commands_restrictions`)"
priority: P1
arztagent: true
status: missing
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

- `own-rule`: conditions with `sender`.
- `defaults`: write permission; fork author for rebase, refresh, squash, update.
- `check-time`: permission is checked when the command runs.
- `template`: `{{author}}` in the default.

## How to get to it (user POV)

- An agent comments `@mergifyio queue` → refusal with a reply.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Simulated forge permission matrix: sender × command × permission.

## Gotchas

- `sender-permission` needs the forge's permission API; a table in the simulator.
