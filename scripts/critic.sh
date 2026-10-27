#!/usr/bin/env bash
# Independent critic (Codex CLI) for a feature, the feature map, or the whole pull request.
#
#   scripts/critic.sh <feature-id>    judge one feature file against the Mergify docs and schema
#   scripts/critic.sh feature-map     judge completeness of the map and the P1 classification
#   scripts/critic.sh pr              judge the whole branch against main
#
# The critic works in its own worktree under /tmp/schlange-critic-<id> on the current commit,
# with fresh context, and must not change code. Its verdict (scripts/critic.schema.json) is
# copied to .critic/<id>.json in this repository; the worktree is removed afterwards.
# Env: SCHLANGE_RESEARCH (default ~/Projects/schlange-research/mergify), CODEX_MODEL (optional).
set -euo pipefail
export CARGO_NET_OFFLINE=true

root="$(git rev-parse --show-toplevel)"
id="${1:?usage: scripts/critic.sh <feature-id|feature-map|pr>}"
research="${SCHLANGE_RESEARCH:-$HOME/Projects/schlange-research/mergify}"
features=".agents/skills/verify-schlange/features"
work="/tmp/schlange-critic-$id"
commit="$(git -C "$root" rev-parse HEAD)"

if [ "$id" != feature-map ] && [ "$id" != pr ] && [ ! -f "$root/$features/$id.md" ]; then
  echo "unknown feature $id (no $features/$id.md)" >&2
  exit 64
fi
[ -d "$research" ] || { echo "Mergify sources missing: $research" >&2; exit 3; }

cleanup() { git -C "$root" worktree remove --force "$work" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
git -C "$root" worktree add --detach "$work" "$commit" >/dev/null
mkdir -p "$work/.research" "$work/.critic"
ln -s "$research" "$work/.research/mergify"

common="You are an independent reviewer of Schlange, an open-source merge queue meant as a
drop-in for Mergify. You have a fresh context and no stake in the code. Do not change any file
in the repository except the verdict file the CLI writes for you; do not commit.

Ground truth: the Mergify documentation and JSON schema in .research/mergify/ (read-only copy,
copyright Mergify). Read AGENTS.md, KONZEPT.md (German), docs/research/ and
$features/README.md first. Run the tests and scenarios yourself
(cargo test --workspace; cargo run -q -p schlange -- sim run scenarios/<file>;
cargo run -q -p schlange -- config validate <file>) and base findings on what you observe.

Judge each sub-feature as conformant, deviating (with evidence: file and line, command and
output, or documentation page) or missing. Check whether every MQ case named by a feature is
covered by a test or scenario; list uncovered IDs in missing_tests. Check the clean room: no
Mergify code and no copied Mergify documentation prose in the repository (short identifiers,
option names and enum values are fine); set clean_room_ok accordingly. Be concrete and terse."

case "$id" in
  feature-map)
    task="Task: judge the feature map in $features/ (not the implementation).
1. Completeness: the inventory _inventory.json is generated from the schema, the OpenAPI file,
   commands*.md and cli*.html. Check that the generator misses nothing important (for example
   configuration paths, attributes, actions, enum values, defaults) and that every entry is
   assigned (cargo test -p schlange-config --test coverage).
2. P1 classification: P1 means used by Arztagent or in effect as a default for it. Arztagent's
   configuration is crates/schlange-config/tests/golden/arztagent.yml; its rules are in
   KONZEPT.md section 3. If ~/Projects/arzt-agent exists, you may read (never change) it with
   git -C ~/Projects/arzt-agent show origin/main:<path> for .github/workflows/ci.yml,
   pr-gate.yml, ai-review.yml, AGENTS.md and PLAN-MERGIFY.md. Report features that should be
   P1 but are not, and P1 features that should not be.
3. Accuracy: feature descriptions against the Mergify docs (behaviour, defaults, dequeue reasons).
Use feature = \"feature-map\", one finding per feature or gap; the overall verdict is
conformant only if nothing major is missing or misclassified."
    ;;
  pr)
    task="Task: judge the whole branch (git diff origin/main...HEAD) against the phase it claims
to complete (see the newest status in $features/README.md and the S- features). Use
feature = \"pr\", one finding per feature or deliverable."
    ;;
  *)
    task="Task: judge the feature $id ($features/$id.md) and its implementation against the
Mergify documentation named in its frontmatter source and the schema. One finding per
sub-feature listed in the file. Use feature = \"$id\"."
    ;;
esac

model_args=()
[ -n "${CODEX_MODEL:-}" ] && model_args=(-m "$CODEX_MODEL")

printf '%s\n\n%s\n' "$common" "$task" | codex exec "${model_args[@]}" \
  -C "$work" -s workspace-write \
  --output-schema "$work/scripts/critic.schema.json" \
  -o "$work/.critic/$id.json" - >"$work/.critic/$id.log" 2>&1 || {
    echo "codex exec failed; log:" >&2
    tail -40 "$work/.critic/$id.log" >&2
    exit 1
  }

mkdir -p "$root/.critic"
python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); d["commit"]=sys.argv[2]; json.dump(d, open(sys.argv[3],"w"), indent=2, ensure_ascii=False)' \
  "$work/.critic/$id.json" "$commit" "$root/.critic/$id.json"
echo "verdict: $(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["verdict"])' "$root/.critic/$id.json") → .critic/$id.json"
