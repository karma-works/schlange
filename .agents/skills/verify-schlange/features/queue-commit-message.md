---
id: queue-commit-message
title: "Commit message on merge"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.commit_message_format"
  - "config:queue_rules.commit_message_format.*"
  - "config:queue_rules.commit_message_template"
  - "default:queue_rules.commit_message_format.*"
  - "enum:title:*"
  - "enum:body:*"
  - "enum:trailers:*"
  - "default:queue_rules.commit_message_template"
unsupported:
  - "config:queue_rules.commit_message_template"
problems:
  - "MQ-20"
source: "https://docs.mergify.com/merge-queue/merge-strategies"
---

# Commit message on merge

A squash produces the commit message in a fixed format. By default it inherits the forge's
behaviour (title and body as GitHub builds them on squash). Title, body and trailers can be
chosen.

## Sub-features

- `default-inherit`: built. No `commit_message_format` (or `inherit`): Schlange sends no title
  or body and GitHub applies the repository's squash settings. The simulated forge defines
  inherit as title `<title> (#n)` and, for more than one commit, one `* <message>` line per
  commit (`crates/schlange-forge/src/sim.rs`, `inherited_message`); the fake GitHub does the
  same, with the body following `squash_merge_commit_message` (`PR_BODY`, `BLANK`).
- `title`: built. `pr-title` is the pull request's title with ` (#<number>)` (our choice).
- `body`: built. `pr-body` the description, `empty` nothing.
- `trailers`: built. After a blank line: `Co-authored-by: Name <email>` per distinct commit
  author other than the pull request's author, `Approved-by: <login>` per reviewer whose latest
  review approves, `Merged-by: <login>` of whoever queued it by command or checkbox, else the
  app. With an inherited body the adapter writes GitHub's body itself and appends them. The
  forms of the trailers are our choice; the docs only name them.
- `template`: missing (listed as unsupported). `commit_message_template` is deprecated and
  removed from Mergify on 2026-09-30; Schlange does not render Jinja2.
- Golden files: one per combination (48), `crates/schlange-core/tests/golden/commit-message/`.

## How to get to it (user POV)

- Look at the commit on `main` after the merge.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p1-commit-message.yaml` → `message` lines with the trailers, `Merged-by: alice` for a command, `Merged-by: schlange[bot]` for a rule, and the inherited message.
- **Golden files.** `cargo test -p schlange-core --test commit_messages`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands commit_message_format_and_the_inherited_default`.

## Gotchas

- "As the forge does it" depends on the repository's squash message setting; test both variants.
- With a batch, each pull request is its own squash commit with its own message.
