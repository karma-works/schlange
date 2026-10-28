---
id: commands
title: "Commands `queue`, `dequeue`, `refresh`"
priority: P1
arztagent: true
status: partial
inventory:
  - "cmd:queue"
  - "cmd:dequeue"
  - "cmd:refresh"
problems:
  - "MQ-50"
source: "https://docs.mergify.com/commands"
---

# Commands `queue`, `dequeue`, `refresh`

Humans steer the queue with comments that start with the bot's name. Arztagent uses `queue` for
constitution pull requests, `dequeue` to stop and `refresh` in operations. The queue acknowledges
a command with a reaction and, where the documentation says so, a reply.

## Sub-features

- `recognition`: built (`crates/schlange-core/src/commands.rs`). The command starts the comment
  (leading blank lines and spaces skipped), prefix `@mergifyio` or `@mergify` in any letter
  case (and `@schlange`), exactly one space, then the command; arguments on the same line, the
  lines below ignored. Our choices: command names are matched without regard to ASCII case;
  quoted arguments may hold spaces; a prefix with an unknown word is answered. Fuzzed in
  `crates/schlange-core/tests/command_parser.rs` (Unicode, case, line breaks, quoting,
  look-alike prefixes).
- `queue`: built. Optional queue name (unknown name: reply, nothing queued). Pending until the
  queue conditions hold and no built-in protection holds the pull request (the pending command
  is a hold in the engine state). A second command with the same or no name is ignored with a
  reply; one naming another queue replaces it, and first takes the pull request out of the
  queue it is in (`PR_MANUALLY_DEQUEUED`, code assumed). Re-queues a dequeued pull request and
  resets what kept it out (manual dequeue, a failed head, attributes that failed the merge
  conditions). `requeue` is the deprecated alias (restriction `requeue`).
- `pending-push`: built. Any new head while the command waits cancels it, whoever pushed it:
  Schlange never pushes to pull requests, and a commit's author (or the login GitHub links to
  its email) is free text the pusher writes, so nothing about a commit can exempt it (critic
  e8a0022 N2; `head_by_queue` is gone). The reply names the pusher only where the forge knows
  it for certain (the simulation); on GitHub it says "someone". After entering the queue a
  push dequeues instead (`PULL_REQUEST_UPDATED`).
- `head-binding`: built. A `queue` command (and a ticked checkbox) vouches for one head. In the
  simulation it is the head when the comment was written. On GitHub, whose comment API names
  no head, it is the head GitHub's activity log (`GET /repos/{o}/{r}/activity`, the record of
  when and by whom a branch moved, which the pusher cannot fake) shows strictly before the
  comment's `created_at` (a tick: its `updated_at`); a push in the same second counts as after
  the comment. Without an answer from the log (a fork's branch lives in another repository's
  log; more than 100 newer moves) it is the head Schlange knew before the comment arrived,
  never one read after it. A head pushed after the comment therefore cancels the command,
  however late Schlange handles it (webhook delay, lost webhook, restart).
- `dequeue`: built. Takes a queued pull request out (`PR_MANUALLY_DEQUEUED`) or cancels a pending
  command (reply); otherwise replies that nothing is queued. Either way after a real dequeue no
  rule queues it again until a new `queue` command (hold `manually-dequeued`).
- `refresh`: built. Reads the pull request again (a `Refresh` read), clears its reported
  evaluation errors and attribute block, replies.
- `reply`: built. 👍 reaction on every command; replies for refusals, errors, a pull request
  left out of the queue, `refresh` and the commands Schlange lacks. A `queue` or `dequeue` that
  takes effect reports in the status comment (docs: commands), not in a reply.
- `once`: built. Every comment is handled once (id and edit version). An edited comment never
  runs as a command, on any path: the webhook `issue_comment.edited` is only read as a ticked
  checkbox in the queue's own comment, by its sender; reconciliation skips every comment whose
  `updated_at` differs from its `created_at`, because GitHub's REST API does not say who
  edited it and anyone with write access may edit anyone's comment (critic e8a0022 N1). A
  command edited before its webhook was handled is lost; its author comments again.
- `recovery`: built. On GitHub, reconciliation reads every page of each open pull request's
  comments (up to the client's limit of 30 pages of 100 per pull request) and runs the unedited
  commands the app has not acknowledged with its reaction (lost webhooks, restarts). A ticked
  checkbox whose webhook was lost is not recovered: the tick is an edit, and only its webhook
  says who made it (fail closed; tick again).

## How to get to it (user POV)

- Comment `@mergifyio queue` (or `dequeue`, `refresh`) on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p1-commands.yaml` → expectations PASS;
  the log shows the reaction, `a queue command by @alice is already waiting`, `@mallory pushed
  new commits`, `dequeue #3 from `default`: PR_MANUALLY_DEQUEUED`, `read #4 before acting`,
  `not a command`, `now targets queue `urgent``.
- **Arztagent's flow.** `target/release/schlange sim run scenarios/p1-arztagent-constitution.yaml`.
- **Engine.** `cargo test -p schlange-core --test commands`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands queue_dequeue_refresh_and_unsupported_commands a_lost_command_webhook_is_recovered_by_reconciliation arztagent_constitution_pull_request_waits_for_a_listed_human`.
- **Attacks.** `cargo test -p schlange-github-fake --test security` (edited comments over webhook,
  reconciliation and a restart; a forged bot author; pushes racing a command; commands behind
  many comments) and `SCHLANGE_GH_ATTACK_CASES=50 cargo test -p schlange-github-fake --test
  attacker` (the property test with an agent as attacker).

## Gotchas

- Schlange must understand `@mergifyio`, otherwise Arztagent's workflows change.
- Whether Mergify matches command names case-insensitively and how it answers an unknown word
  is not documented; observe it before claiming conformance.
- The texts of replies and the status comment are Schlange's own.
- A comment cannot prove which commits its author looked at. Binding to the head before the
  comment's time is the best GitHub offers: a human who comments within the same second as a
  push sees the command cancelled and comments again. Whether GitHub's activity log lists
  every move of a pull request's branch promptly (and needs no permission beyond `contents:
  read`) is to be confirmed in a sandbox.
