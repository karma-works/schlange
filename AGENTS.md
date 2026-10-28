# Schlange: working rules for agents

Schlange is an open-source merge queue that replaces Mergify for Arztagent without changing
its `.mergify.yml`. Architecture, phases and invariants: [KONZEPT.md](KONZEPT.md). Research:
[docs/research/](docs/research/) (problem catalogue MQ-01…MQ-53, Mergify weaknesses W-01…W-26).

The repository language is English: code, identifiers, docs, feature files, commit messages.
KONZEPT.md and docs/research/ predate that rule and are still in German.

## Layout

| Path | What |
|---|---|
| `crates/schlange-config` | loading, validation stages (YAML, schema, conditions, support), condition parser, feature map, configuration file names and the "configuration changed" check (`config_file`) |
| `crates/schlange-core` | state machine without I/O: `(state, event) → (state, decisions)`, condition evaluation, comment commands (`commands` parser, `control`), built-in protections as holds (`protections`), cosmetic decisions: labels, status and checkbox comments, replies (`cosmetics`), squash commit messages (`message`) |
| `crates/schlange-forge` | `Forge` trait, simulated forge (in-memory git, scripted CI, webhook faults), `localgit` forge (real git repositories on disk, checks as processes in checkouts; cargo feature `localgit`), invariant checker I0–I5, scenario runner, Rust DSL, property tests |
| `crates/schlange-github` | GitHub App adapter: REST client (rustls, ring), webhooks, a bounded HTTP listener, `GithubForge`, the `serve` loop with reconciliation, SQLite persistence (journal, snapshots, dedup, audit), configuration validation, doctor checks, permission list |
| `crates/schlange-github-fake` | test support: fake GitHub over HTTP (simulated git, signed webhooks, API and webhook faults), tester DSL, end-to-end tests |
| `crates/schlange` | the binary and its CLI (`config`, `sim`, `support-matrix`, `doctor`, `serve`) |
| `tools/inventory` | builds the feature inventory from the pinned Mergify sources |
| `tools/fetch-mergify-sources`, `tools/mergify-sources.lock` | downloads and pins the sources |
| `.agents/skills/verify-schlange/` | verification skill; `features/` is the feature map |
| `scenarios/` | scenarios for `schlange sim run`; `scenarios/configs/` holds their configurations |
| `docs/mq-coverage.md` | which MQ case has which test |
| `.cargo/config.toml` | `LIBSQLITE3_FLAGS`: the bundled SQLite without FTS, R-tree, JSON, extension loading (binary and image size) |
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
9. **Cosmetics never gate landing.** Labels, comments, reactions and reported checks are
   `Decision::Cosmetic`: never read back as a condition (the queue's own labels are left out of
   the attributes that can block a pull request), sent by the GitHub adapter with the lowest
   priority. The texts are Schlange's own, never Mergify's.
10. **Every hold has a reason.** Whatever keeps an open pull request out of the queue (a
   protection, a waiting command, a manual dequeue) is a `Hold` in `State::holds`.

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
SCHLANGE_GH_ATTACK_CASES=50 cargo test -p schlange-github-fake --test attacker  # an agent with write access as attacker
cargo test -p schlange-github-fake --test security                      # critic reproductions: edits, forged authors, races, 405/409
cargo test -p schlange-github-fake --test e2e_commands                  # commands, checkboxes, labels, protections, commit messages
cargo test -p schlange-core --test commands --test command_parser       # engine-level commands and the fuzzed parser
docker build -t schlange:dev . && scripts/image-size.sh
```

Golden files: `UPDATE_GOLDEN=1 cargo test -p schlange-config --test golden` (validator),
`UPDATE_GOLDEN=1 cargo test -p schlange-forge --test scenarios` (scenario event logs),
`UPDATE_GOLDEN=1 cargo test -p schlange-core --test commit_messages` (squash messages) and
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
- Threat model (Arztagent): an agent with write access may push, comment, label and edit
  anyone's comments. So an edited comment never runs as a command (only its webhook names the
  editor, and it counts only as a ticked checkbox by that sender); a commit's author never
  vouches for anything (any new head cancels a waiting command); a command vouches for the
  head GitHub's activity log shows before the comment, never one read later; landing is
  decided by the engine alone (a transient 405/409 goes back to it, never retried in the
  adapter); a truncated file list fails every `files` condition. Tests:
  `cargo test -p schlange-github-fake --test security --test attacker`
  (`SCHLANGE_GH_ATTACK_CASES=50` for a long run). Residuals: `commands.md` (`head-binding`,
  Gotchas).

Scenarios can comment (`comment: {pr, by, body}`), tick the queue's checkboxes
(`tick: {pr, by, checkbox: queue|requeue}`), push as someone else (`push: {pr, by}`), edit the
description (`edit_body`), give logins permissions (`permissions:`) and keep the configuration
in `.mergify.yml` on the default branch (`config_in_repo: true`); `expect` takes
`held: <kind or text>`. The simulation's time 0 is 2026-01-01T00:00:00Z.

## Exit codes of `schlange`

| Code | Meaning |
|---|---|
| 0 | valid / scenario passed / doctor without FAIL |
| 1 | invalid configuration / scenario failed / doctor FAIL |
| 2 | valid for Mergify but uses features Schlange does not support (`config validate --strict`) |
| 3 | environment problem: no schema, unreadable file, broken scenario |
| 64 | usage error on the command line (`EX_USAGE`); JSON on stdout with `--json` |

## Git

- Work on a branch, open a pull request against `main`. Never push to `main`.
- Do not touch Arztagent (`~/Projects/arzt-agent`): read only.
