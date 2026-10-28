---
id: queue-modes
title: "Queue modes `parallel` and `isolated`"
priority: P2
arztagent: false
status: partial
inventory:
  - "enum:mode:parallel"
  - "enum:mode:isolated"
problems:
  - "MQ-44"
  - "MQ-48"
source: "https://docs.mergify.com/merge-queue/queue-modes"
---

# Queue modes `parallel` and `isolated`

Besides the serial chain there is a parallel mode with independent scopes and an isolated mode in
which every pull request is tested on its own.

## Sub-features

- `isolated`: built. Every car is built on the base alone (no pull requests ahead), cars do not depend on one another, any green car lands when no other landing is in flight, and a failing car discards no other car. Batches are grouped by similarity as in serial mode (`crates/schlange-core/src/queue.rs`, `Mode::Isolated`).
- `parallel`: built for file-pattern scopes. A batch holds pull requests with exactly the same scopes (and barrier flag); it depends on the last car ahead in each of its scopes and is built on top of all of them (`Car::deps`); a car lands once every car it depends on has landed; a failure discards only the cars that depend on the failed one. A barrier runs alone.
- `drift`: built. A car that does not depend on a car that landed was tested on a base without it: it lands as its tested change on the new base (`Car::drifted`, no tree halt). I0 checks such a landing as the tested change applied to the actual parent, with only other lanes' landings (isolated: any; parallel: landings in scopes the car does not touch) or, with `reset_on_external_merge: never`, external commits in between.
- `bisection`: partial. A failed batch of several pull requests is split as in serial mode; while it is split every other car is rebuilt after it (our simplification).
- `capacities`: missing (`scopes.capacities`, `scopes.default_capacity`).
- `fast-forward`: refused as documented: `config validate` rejects `fast-forward` outside serial mode (stage 4), and the property generators never combine them.

## How to get to it (user POV)

- Option `merge_queue.mode`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Isolated.** Run `target/release/schlange sim run scenarios/p2-isolated.yaml`: three cars `base 5a8b940 + #n` at once; #1 fails (`CHECKS_FAILED`) without discarding the others; #2 and #3 land. Exit 0.
- **Parallel with scopes.** Run `target/release/schlange sim run scenarios/p2-parallel-scopes.yaml`: `(checks #1 #3)` for `api`, `(checks #2)` for `web` on the base alone, `+ #1 #3 #2 #4 (checks #4)` for the pull request in both scopes, then the barrier #5 alone. Exit 0.
- **Real git.** `cargo test -p schlange-forge --features localgit --test localgit` runs both.
- **Property tests.** The generators draw `isolated` and `parallel` (with scopes and a barrier file); I0–I5 hold. A reset that kept independent cars was found this way (`cargo test -p schlange-forge --test p2 isolated_car_never_lands_over_an_unseen_external_push`).

## Gotchas

- Never test overlapping scopes independently (MQ-48): cars that share a scope are chained.
- Isolated mode accepts the documented risk of semantic conflicts between independent pull requests.
