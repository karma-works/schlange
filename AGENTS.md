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
| `crates/schlange-forge` | `Forge` trait, simulated forge with virtual clock, scenario runner |
| `crates/schlange` | the binary and its CLI |
| `tools/inventory` | builds the feature inventory from the pinned Mergify sources |
| `tools/fetch-mergify-sources`, `tools/mergify-sources.lock` | downloads and pins the sources |
| `.agents/skills/verify-schlange/` | verification skill; `features/` is the feature map |
| `scenarios/` | scenarios for `schlange sim run` |
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
   queue exists. Never weaken an invariant check to make a scenario pass.
4. **Evidence.** A feature counts as working only with evidence from the `verify-schlange`
   skill (`.agents/skills/verify-schlange/SKILL.md`): build, doctor, drive, evidence.
5. **Critic acceptance.** `status: conformant` in a feature file needs a green verification and
   a critic verdict `conformant` (`scripts/critic.sh <feature-id>`, result in `.critic/`).
   `partial` means some sub-features are built and say so in the file.
6. **Feature map.** Every inventory entry belongs to exactly one feature file. When Mergify's
   schema changes: `tools/fetch-mergify-sources --update`, `cargo run -p inventory`, assign the
   new entries, keep the coverage test green.
7. **Core without side effects.** `schlange-core` does no I/O. Side effects go through a forge
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
docker build -t schlange:dev . && scripts/image-size.sh
```

Golden files: `UPDATE_GOLDEN=1 cargo test -p schlange-config --test golden`, then review the diff.

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
