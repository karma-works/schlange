---
id: conditions-files
title: "Attribute `files` with regex"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:files"
problems:
  - "MQ-07"
  - "MQ-52"
source: "https://docs.mergify.com/configuration/conditions"
---

# Attribute `files` with regex

`files` is the list of paths changed by the pull request. Arztagent excludes constitution paths
with a negated regex with lookahead, which needs Python-compatible regex semantics.

## Sub-features

- `list`: built. The simulation diffs trees, so a moved file shows at both paths; the GitHub adapter reads `/pulls/{n}/files` and counts a renamed file at its `previous_filename` and its `filename` (docs: a renamed file counts at both paths), so `git mv` out of a guarded directory cannot pass a `-files` gate (security fix, critic 1abc; regression `cargo test -p schlange-github-fake --test client a_renamed_constitution_file_still_trips_arztagents_files_gate` with Arztagent's constitution gate).
- `count`: partial. `#files` counts both paths of a rename; GitHub's own count, which the docs name for `#files`, counts it once.
- `regex`: built. Search like Python's `re.search`, lookahead through `fancy-regex`; a pattern that fails while matching (backtrack limit) is an evaluation error and fails closed. A difference test against Python is missing.
- `negation`: built. `-files ~= …` means "no file matches".
- `glob`: built as the docs describe (`crates/schlange-config/src/glob.rs`): `**` as a whole segment matches zero or more segments (`**/*.py` matches root files), `*` and `?` stay within a segment, `[seq]`, `[!seq]`, nested `{a,b}` with `\{ \} \,` escapes, empty branches dropped, at most 128 alternatives and 32 levels; an invalid glob is a configuration error.
- `added-files`, `removed-files`, `modified-files`: missing (see `conditions-files-other`).

## How to get to it (user POV)

- A pull request changes `.github/workflows/ci.yml` → the rule does not apply, a human must queue it.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; `#2` (touches `.github/workflows/ci.yml`) is not queued, `#9` (exception path under `.agents/skills/verify-arztagent/features/`) is queued.

## Gotchas

- A new push changes the file list; evaluation is bound to the head SHA (MQ-07).
