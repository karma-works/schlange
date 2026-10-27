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
- `pending-push`: built. A push by anyone but the queue while the command waits cancels it, with
  a reply naming the pusher (GitHub: the login of the head commit's author; the simulation:
  who pushed). The queue's own updates do not cancel it (`head_by_queue`; Schlange does not
  update pull requests yet, so this path is not reachable). After entering the queue a push
  dequeues instead (`PULL_REQUEST_UPDATED`). The command is bound to the head when it was
  written (simulation) or when Schlange read the comment (GitHub, whose API does not tie a
  comment to a head).
- `dequeue`: built. Takes a queued pull request out (`PR_MANUALLY_DEQUEUED`) or cancels a pending
  command (reply); otherwise replies that nothing is queued. Either way after a real dequeue no
  rule queues it again until a new `queue` command (hold `manually-dequeued`).
- `refresh`: built. Reads the pull request again (a `Refresh` read), clears its reported
  evaluation errors and attribute block, replies.
- `reply`: built. 👍 reaction on every command; replies for refusals, errors, a pull request
  left out of the queue, `refresh` and the commands Schlange lacks. A `queue` or `dequeue` that
  takes effect reports in the status comment (docs: commands), not in a reply.
- `once`: built. Every comment is handled once (id and edit version); edits of a command do
  not run it again. On GitHub, reconciliation finds commands among the 100 newest comments that
  the app has not acknowledged with its reaction (lost webhooks, restarts).

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

## Gotchas

- Schlange must understand `@mergifyio`, otherwise Arztagent's workflows change.
- Whether Mergify matches command names case-insensitively and how it answers an unknown word
  is not documented; observe it before claiming conformance.
- The texts of replies and the status comment are Schlange's own.
