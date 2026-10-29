---
id: queue-landing
title: "Landing as compare-and-swap with evidence"
priority: P1
arztagent: true
status: partial
inventory: []
problems:
  - "MQ-02"
  - "MQ-05"
  - "MQ-06"
  - "MQ-31"
  - "MQ-32"
  - "MQ-35"
  - "MQ-07"
  - "MQ-11"
  - "MQ-29"
  - "MQ-30"
source: "https://docs.mergify.com/merge-queue/lifecycle"
---

# Landing as compare-and-swap with evidence

Landing happens only when the base still is exactly the state that was tested and the pull request
head has not changed. Afterwards Schlange checks that the landed tree equals the tested tree. A
journal entry precedes every side effect.

## Sub-features

- `fresh-read`: built. A green front car first asks for a fresh read (`Refresh`) of its pull requests, its draft, the base and its branch; only if queue and merge conditions still hold on that read does the engine decide `Land` (MQ-11: a lost label webhook cannot let a pull request land).
- `cas`: built. `Land` carries the expected base SHA; the forge squashes each pull request and moves the base by compare-and-swap, never forced. A moved base refuses the landing (`BaseMoved`) and resets the queue. On `localgit` the compare-and-swap is git's own: `git update-ref refs/heads/<base> <new> <expected>` (creation of a queue branch expects the all-zero id), so a stale expectation is refused by git under the ref lock (`localgit::tests::commits_trees_and_compare_and_swap`; scenario `p1-base-moved` on real git).
- `head-bound`: built. Every queue entry and car is bound to the head SHA it was queued and tested with; a different head dequeues (`PULL_REQUEST_UPDATED`) and the forge refuses to land a head that is not the tested one (I1).
- `tree-check`: built. After landing, the engine compares the landed tree with the tested tree; on a difference it halts the queue of that base (`Halt`), MQ-02. The invariant checker independently checks every base commit against the tested prefix trees (I0).
- `webhooks-are-hints`: built in the simulation. A delivered webhook only names a pull request or branch; the runner reads the truth and gives it to the engine. Faults drop, duplicate and delay deliveries; a periodic reconciliation reads everything (MQ-29, MQ-30).
- `cas-github`: partial. GitHub's merge API has no expected base. The adapter reads the pull request and the base immediately before every merge call and refuses (`BaseMoved`, `HeadChanged`) on any difference; no merge call is ever sent after a wait (rate limit, backoff) without these reads, and a failed landing is re-decided by the engine from fresh reads, never retried by the adapter. A push inside the one-request window between the base read and GitHub executing the call is caught afterwards by the parent and tree check (halt), not prevented. Why the ref-update compare-and-swap is not used: `crates/schlange-github/src/api.rs`.
- `journal`, `crash-restart`: built. In the simulation (`MemoryJournal`, `--test crashes`) and in `schlange serve` (SQLite, `crates/schlange-github/src/store.rs`): checkpoint before the side effects of a step, intent before each side effect; a restart resolves landings in doubt from the forge (`schlange-github-fake --test serve a_crash_mid_landing_recovers_the_queue_and_never_lands_twice`).
- `unclear-error`: built (MQ-35): lying 5xx in the simulation's fault mode, on real git and on the fake GitHub.
- `one-writer`: partial. `serve` uses a SQLite lease and a GitHub fence across processes (MQ-32); the fake-GitHub lease tests exercise takeover and a paused former leader. The client checks the write gate immediately before each HTTP write attempt, including retries and same-host redirects. Metadata read/write errors and mismatched predecessors do not establish trusted journal lineage through an ordinary renewal (`schlange-github-fake --test lease`). Metadata updates are not transactionally bound to lease ownership, and an initially absent local and remote fence is still assumed fresh. A remote fence check and the subsequent write are not atomic, and matching holder names or unfenced writers remain limitations.

## How to get to it (user POV)

- Invisible to users; visible in the evidence (`S-AUDIT`).

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Every scenario.** `target/release/schlange sim run scenarios/p1-base-moved.yaml` shows `read #1 #1002 main mergify/merge-queue/… before acting`, then `land car 5: squash #1 onto main if it is still 02fd98e`, `car 5 landed #1 as …`; the report lists `I0  checked`, `I1  checked` and `I5  checked` with the disturbed run's dropped and duplicated deliveries.
- **Refusals and halt.** `cargo test -p schlange-core --test queue` (`landed_tree_differs_from_tested_tree_halts_the_queue`, `landing_refused_because_the_base_moved_resets_the_queue`, `landing_refused_because_the_head_moved_dequeues`, `a_stale_green_is_rechecked_before_landing`, `a_label_seen_only_in_the_fresh_read_prevents_landing`).
- **Random.** `cargo test -p schlange-forge --test properties` (I0 and I1 after every step).
- **Real git.** `cargo test -p schlange-forge --test localgit` and `--test localgit_properties`: the checker reads the base's first-parent history (`git rev-list`) and each landed tree from the repository on disk.
- **GitHub (fake).** `cargo test -p schlange-github-fake --test landing` (no landing after a dequeue, none on a foreign base after a rate-limit wait, refusals) and `--test properties` (I0, I1, I3 under random faults).

## Gotchas

- No Mergify configuration path, but the core of the drop-in promise.
- The tree check compares the last landed commit with the car's tree. Commits inside a batch are checked by the invariant checker against the car's prefix trees, not by the engine.
- A halted queue keeps its pull requests queued; I2 reports them as held by the halt, with the reason.
