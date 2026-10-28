---
id: action-label
title: "Action `label`"
priority: P2
arztagent: false
status: partial
inventory:
  - "action:label"
  - "config:pull_request_rules.actions.label"
  - "config:pull_request_rules.actions.label.*"
  - "config:defaults.actions.label"
  - "config:defaults.actions.label.*"
  - "default:pull_request_rules.actions.label.*"
  - "default:defaults.actions.label.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/label"
---

# Action `label`

Adds, removes or toggles labels; the labels are real labels that conditions read (unlike the queue's own labels). Arztagent does not use the action.

## Sub-features

- `run`: built. Level-triggered (docs: workflow, "How rules are evaluated"): runs on every evaluation and asks only for the difference to what the forge shows; an outstanding request is not repeated. Someone removing a label the rule adds gets it back.
- `options`: built: `add`, `remove`, `remove_all` (never removes the queue's own labels), `toggle` (added while the rule matches, removed otherwise). Label names are simple templates.
- `loop guard`: built. Within one evaluation the first rule that changes a label owns it; a later rule's opposite change is dropped. Across evaluations, on the tenth time within an hour that the actions reverse their own last change of a label on a pull request, they stop changing that label there until the configuration changes (Schlange also counts a rule that undoes itself; the stop is reported once in a comment on the pull request and shows in `/status` as `actions.frozen_labels`).

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-actions-label-comment.yaml`: the WIP toggle, a label put back, a rule-added label that the queue conditions read.
- **Engine.** `cargo test -p schlange-core --test actions`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_actions` (serve against the fake GitHub over HTTP).
- **Crashes.** `cargo test -p schlange-forge --test action_crashes`: a crash before and after every side effect of the action scenarios posts nothing twice and leaves the same labels, assignees, checks and copies.
- **Properties.** `SCHLANGE_PBT_CASES=400 cargo test -p schlange-forge --test properties`: random rules with this action run next to the queue; I0–I4 and the innocence oracle hold, no action comment is posted twice.

## Gotchas

- A label the action adds can queue or dequeue a pull request: it is a condition input like any other.
- Labels that do not exist are created by GitHub on first use (its own colour).
