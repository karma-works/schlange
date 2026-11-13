---
name: verify-schlange
description: "Prove that a Schlange change works: build the binary (and the scratch image), run `schlange doctor`, drive the mapped CLI paths and scenarios in the simulated forge, capture evidence, and hand the feature to the critic. Use before saying a feature works, before opening a PR, and whenever someone asks to verify, smoke-test or demo Schlange."
---

# Verify Schlange

You are proving behaviour to someone who will not rerun it. A green unit test is not proof
that the binary does the thing. Proof is the real CLI path on the release binary, its exit code
and output, the scenario's event log and invariant report, all captured as files.

Paths are relative to the repository root. The driver is
`.agents/skills/verify-schlange/scripts/verify.sh` (`$V` below). One run:

```bash
.agents/skills/verify-schlange/scripts/verify.sh all --image --feature <feature-id>
```

## 0. Choose what to run

| Level | When | Command |
|---|---|---|
| Static | always | `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` |
| Tests | always | `cargo test --workspace` (coverage test, golden files, scenarios) |
| Drive | anything a user can trigger: CLI, validation output, scenarios | `$V all` |
| Image | Dockerfile, dependencies, doctor, anything that must run in the container | `$V all --image` |

## 1. Launch

`$V launch [--image]` builds `target/release/schlange` with `--locked`; with `--image` also the
container `schlange:verify` (musl, `FROM scratch`, user 65532) and checks its size against
`image-budget.txt` with `scripts/image-size.sh`. Needs `.research/mergify/` for validation
(`tools/fetch-mergify-sources`); the image itself contains no Mergify file.

## 2. Doctor

`$V doctor [--image]` runs `schlange doctor` on the binary and, with `--image`, inside the
container read-only with all capabilities dropped (`no-new-privileges` is left out on the VPS, where it breaks exec for every image). Green means no FAIL line. The container's embedded clean-room runtime validator reports
OK without an upstream schema; SKIP lines name checks not built yet. A FAIL is a
reason to fix, not to drive anyway.

## 3. Drive

`$V drive` runs, on the release binary:
- `config validate` on Arztagent's configuration (exit 0), on broken files (exit 1 with path,
  line and column), `--strict` on an unsupported one (exit 2), without schema (exit 3);
- every `scenarios/*.yaml` through `sim run` (exit 0 when all expectations hold);
- `support-matrix`.

The feature map in [`features/`](features/README.md) is the source of truth: what each feature
is, how a user reaches it, how to drive it, and the traps. Before verifying a change, find the
features it touches there. A feature without a scenario gets one first (rule "test first" in
`AGENTS.md`); copy the shape of `scenarios/p0-queue-conditions.yaml`. Never weaken an
expectation or an invariant to make a run pass.

## 4. Evidence

Every run writes `evidence/<timestamp>-<feature>/` (git-ignored): one `.txt` per step with the
command output, `.json` for machine-readable outputs, and `proof.md` with one PASS/FAIL line
per claim and the commit. Summarise `proof.md` in the PR. Then ask the critic:
`scripts/critic.sh <feature-id>`; only its verdict `conformant` allows `status: conformant`.

Proof standards: drive the real CLI path on the release binary, not library calls or test
helpers; capture the command and the resulting output and exit code, not only a summary; for
scenarios keep the event log, which is the observable side effect of the simulated forge.
`$V` drives the simulated forge only. GitHub-side behaviour is proven against the fake GitHub
(`cargo test -p schlange-github-fake`) and, where a feature file says it needs a sandbox, with
the `sandbox-e2e` skill.

## 5. Cleanup

`$V cleanup` removes only containers labelled with this run's evidence directory and the image
`schlange:verify`. It never touches other images, containers or `.research/`, and never the
evidence: after cleanup, `evidence/<run>/proof.md` must still exist. Isolation: every scenario
runs in its own in-memory forge and the binary keeps no state, so runs can go side by side;
the only shared name is the image tag `schlange:verify`, so do not run two `--image` runs at
once.

## 6. Helpers

| Helper | Invocation |
|---|---|
| `scripts/verify.sh` | `.agents/skills/verify-schlange/scripts/verify.sh <launch\|doctor\|drive\|cleanup\|all> [--image] [--feature <id>] [--evidence <dir>]`; exit 0 only when every claim in `proof.md` passed |
| `scripts/image-size.sh` (repo) | `scripts/image-size.sh [image]`; compares with `image-budget.txt` |
| `scripts/critic.sh` (repo) | `scripts/critic.sh <feature-id\|feature-map\|pr>`; verdict in `.critic/<id>.json` |
| `tools/fetch-mergify-sources` (repo) | `tools/fetch-mergify-sources [--update\|--offline]` |

Not covered by `$V`: GitHub App permissions, webhooks and rulesets (fake GitHub tests, then
`sandbox-e2e`), and the image under a real orchestrator (read-only root, volume `/data`).
