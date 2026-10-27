# Schlange: working rules for agents

Schlange is an open-source merge queue that replaces Mergify for Arztagent without changing
its `.mergify.yml`. Architecture, phases and invariants: [KONZEPT.md](KONZEPT.md). Research:
[docs/research/](docs/research/) (problem catalogue MQ-01…MQ-53, Mergify weaknesses W-01…W-26).

The repository language is English: code, identifiers, docs, feature files, commit messages.
KONZEPT.md and docs/research/ predate that rule and are still in German.

## Layout

| Path | What |
|---|---|
| `crates/schlange-config` | loading, validation stages (YAML, schema, conditions, support), condition parser, feature map |
| `crates/schlange-core` | state machine without I/O: `(state, event) → (state, decisions)`, condition evaluation |
| `crates/schlange-forge` | `Forge` trait, simulated forge (in-memory git, scripted CI, webhook faults), `localgit` forge (real git repositories on disk, checks as processes in checkouts; cargo feature `localgit`), invariant checker I0–I5, scenario runner, Rust DSL, property tests |
| `crates/schlange-github` | GitHub App adapter: REST client (rustls, ring), webhooks, a bounded HTTP listener, `GithubForge`, the `serve` loop with reconciliation, SQLite persistence (journal, snapshots, dedup, audit), configuration validation, doctor checks, permission list |
| `crates/schlange-github-fake` | test support: fake GitHub over HTTP (simulated git, signed webhooks, API and webhook faults), tester DSL, end-to-end tests |
| `crates/schlange` | the binary and its CLI (`config`, `sim`, `support-matrix`, `doctor`, `serve`) |
| `tools/inventory` | builds the feature inventory from the pinned Mergify sources |
| `tools/fetch-mergify-sources`, `tools/mergify-sources.lock` | downloads and pins the sources |
| `.agents/skills/verify-schlange/` | verification skill; `features/` is the feature map |
| `scenarios/` | scenarios for `schlange sim run`; `scenarios/configs/` holds their configurations |
| `docs/mq-coverage.md` | which MQ case has which test |
| `scripts/critic.sh` | independent critic (Codex CLI) |
| `.critic/` | critic verdicts, checked in |

## Rules

1. **Clean room.** No code and no documentation text from Mergify in this repository. Behaviour
   comes only from the public docs, the schema and observed behaviour, written in our own
   words. The Mergify sources live in `.research/mergify/` (git-ignored, copyright Mergify);
   fetch them with `tools/fetch-mergify-sources`. Do not use the Mergify CLI: its licence forbids
   use for competing products. The inventory holds identifiers only; prose defaults appear as a
   hash.
2. **Test first.** Before implementing a feature, write its scenario (`scenarios/`) and tests
   from the feature file and the MQ cases it names. Then implement.
3. **Invariants.** I0–I5 (KONZEPT.md, section 6) must hold after every simulated step once the
   queue exists. Never weaken an invariant check to make a scenario pass. I0 follows Mergify's
   batch semantics (each commit of a `squash` batch is a tested prefix tree; checks ran on the
   batch tip), is strict per commit for batch size 1, never accepts a vacuous "green" (without a
   named check the conditions must have held on the queue branch's draft), and with
   `reset_on_external_merge: never` checks the tested change on the actual parent. The
   engine's `Refresh` reads must be consistent reads in every forge adapter (I1 depends on it).
   Fault mode (`crates/schlange-forge/src/faults.rs`): forge faults and engine crashes; the state
   is journaled before every side effect (`schlange_core::journal`).
4. **Evidence.** A feature counts as working only with evidence from the `verify-schlange`
   skill (`.agents/skills/verify-schlange/SKILL.md`): build, doctor, drive, evidence.
5. **Critic acceptance.** `status: conformant` in a feature file needs a green verification and
   a critic verdict `conformant` (`scripts/critic.sh <feature-id>`, result in `.critic/`).
   `partial` means some sub-features are built and say so in the file.
6. **Feature map.** Every inventory entry belongs to exactly one feature file. When Mergify's
   schema changes: `tools/fetch-mergify-sources --update`, `cargo run -p inventory`, assign the
   new entries, keep the coverage test green.
7. **No real GitHub in tests.** GitHub behaviour is tested against `schlange-github-fake`;
   tests and agents never call api.github.com, register apps or use real tokens.
8. **Core without side effects.** `schlange-core` does no I/O. Side effects go through a forge
   adapter (`sim` → `localgit` → `forgejo` → `github`).

## Commands

```bash
tools/fetch-mergify-sources                 # sources into .research/mergify/, checked against the lock
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                      # includes the coverage test and golden files
cargo run -q -p inventory -- --check        # inventory matches the pinned sources
cargo run -q -p schlange -- config validate <file>
cargo run -q -p schlange -- sim run scenarios/p0-queue-conditions.yaml
SCHLANGE_PBT_CASES=2000 cargo test -p schlange-forge --test properties   # long property run
cargo test -p schlange-forge --test localgit                             # scenarios + matrix on real git (needs git ≥ 2.38)
SCHLANGE_LOCALGIT_PBT_CASES=50 cargo test --release -p schlange-forge --test localgit_properties
cargo run -q -p schlange --features localgit -- sim run --forge localgit scenarios/p1-bisection.yaml
cargo test -p schlange-github-fake                                      # schlange serve against the fake GitHub over HTTP
SCHLANGE_GH_PBT_CASES=50 cargo test -p schlange-github-fake --test properties   # long property run on the fake GitHub
docker build -t schlange:dev . && scripts/image-size.sh
```

Golden files: `UPDATE_GOLDEN=1 cargo test -p schlange-config --test golden` (validator),
`UPDATE_GOLDEN=1 cargo test -p schlange-forge --test scenarios` (scenario event logs) and
`UPDATE_GOLDEN=1 cargo test -p schlange-github-fake --test golden_decisions` (the engine's
decisions against the fake GitHub), then review the diff.

## `schlange serve`

| Flag | Environment | Default | What |
|---|---|---|---|
| `--app-id`, `--private-key` | `SCHLANGE_GITHUB_APP_ID`, `SCHLANGE_GITHUB_PRIVATE_KEY` | – | the GitHub App |
| `--repo` | `SCHLANGE_REPOS` | – | `owner/name`, comma-separated |
| `--webhook-secret-file` | `SCHLANGE_WEBHOOK_SECRET_FILE` or `SCHLANGE_WEBHOOK_SECRET` | – | at least 16 bytes; `--webhook-secret` on the command line works but warns (visible in `ps`) |
| `--data-dir` | `SCHLANGE_DATA_DIR` | `/data` | `schlange.db` (SQLite, WAL): journal, snapshots, `auto` history, webhook dedup, audit |
| `--schema` | `SCHLANGE_SCHEMA` | `.research/mergify/schema.json` upwards | `serve` does not start without it: the configuration is validated like `config validate` |
| `--admin-token-file` | `SCHLANGE_ADMIN_TOKEN` | off | bearer token for `GET /status`; without it `/status` answers 404 |
| `--listen` | `SCHLANGE_LISTEN` | `0.0.0.0:8080` | `POST /webhook`, `GET /healthz`, `GET /status` |
| `--api-url` | `SCHLANGE_GITHUB_API_URL` | `https://api.github.com` | must be `https://` |
| `--insecure-test-api` | `SCHLANGE_INSECURE_TEST_API` | off | allows `http://` API URLs; tests against the fake GitHub only |
| `--reconcile-every`, `--config`, `--check-name`, `--installation-id` | … | | see `schlange serve --help` |

Security notes:

- The configuration comes from the default branch only; an invalid one is never applied (the
  last valid one stays, the error is in the log, `/status` and `schlange doctor`).
- Requests and the token go only to the API host (`Link` pages and redirects elsewhere are
  refused); installation tokens are downscoped to the served repositories and permissions.
- The listener bounds every request (thread pool, read timeout, deadline, size limits); put
  TLS in front of it.
- Landing on GitHub is not a true compare-and-swap (the merge API has no expected base); see
  `crates/schlange-github/src/api.rs` for the guards and the trade-off.
- Tests never use real tokens; the fake GitHub's test key is in
  `crates/schlange-github-fake/fixtures/`.

## Exit codes of `schlange`

| Code | Meaning |
|---|---|
| 0 | valid / scenario passed / doctor without FAIL |
| 1 | invalid configuration / scenario failed / doctor FAIL |
| 2 | valid for Mergify but uses features Schlange does not support (`config validate --strict`) |
| 3 | environment problem: no schema, unreadable file, broken scenario |

## Git

- Work on a branch, open a pull request against `main`. Never push to `main`.
- Do not touch Arztagent (`~/Projects/arzt-agent`): read only.
