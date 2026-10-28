---
id: action-merge
title: "Action `merge`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:merge"
  - "config:pull_request_rules.actions.merge"
  - "config:pull_request_rules.actions.merge.*"
  - "config:defaults.actions.merge"
  - "config:defaults.actions.merge.*"
  - "default:pull_request_rules.actions.merge.*"
  - "default:defaults.actions.merge.*"
  - "enum:method:*"
unsupported:
  - "config:pull_request_rules.actions.merge.commit_message_format"
  - "config:pull_request_rules.actions.merge.commit_message_format.*"
  - "config:pull_request_rules.actions.merge.commit_message_template"
  - "config:pull_request_rules.actions.merge.merge_bot_account"
  - "config:defaults.actions.merge.commit_message_format"
  - "config:defaults.actions.merge.commit_message_format.*"
  - "config:defaults.actions.merge.commit_message_template"
  - "config:defaults.actions.merge.merge_bot_account"
  - "enum:method:rebase"
problems:
  - "MQ-06"
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/merge"
---

# Action `merge`

Merges the pull request directly, outside the queue. Arztagent does not use the action.

## Sub-features

- `run`: built. Edge-triggered (docs: workflow, "How rules are evaluated"). A landing path outside the queue, so it is decided like a landing: the engine first reads the pull request again (`Refresh`) and merges only on that fresh read (I1), when the rule still matches without an evaluation error (a truncated file list fails closed), the pull request is open, ready, not queued, not held (protections, a waiting `queue` command), and neither the pause, a freeze nor a merge protection holds it (stricter than the docs for the freeze, which they leave to the queue). The merge call names the head of that read (GitHub refuses another head). A merge that fails or is in doubt after a restart is never resent, neither by the adapter nor by the engine: the engine decides again on a new read.
- `options`: `method` `squash` (default), `merge`; `fast-forward` in the simulated forge only; `rebase`, `commit_message_format`, `commit_message_template`, `merge_bot_account` not built.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-branches.yaml`: the merge counts as a merge outside the queue (like one by hand); I0 holds.

## Gotchas

- Branch protection and rulesets are left to GitHub, which refuses the merge; the refusal is logged.
