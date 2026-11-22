---
name: codebase-safety
description: "Invoke every time you create or change a GitHub Actions workflow (.github/workflows/*.yml, composite actions), an agent instruction file (AGENTS.md, CLAUDE.md, .cursorrules, skills, hooks, .claude/settings.json, MCP configs) or a shell script (*.sh, files with a shell shebang, justfile/Makefile recipes) — before you commit. Also invoke for a full codebase and environment safety audit (\"Safety-Audit\", \"ist die Umgebung sicher für Agenten?\", \"audit the dev setup\"). Checks for blast radius: production data reachable from tests, broad credentials, unguarded recursive deletes, destroyable infrastructure, prose-only rules, unprotected branches, risky agent configs, CI that runs agents on untrusted input with secrets, and tools that spend money or send messages without limits."
---

# Codebase and environment safety

Adapted in our own words from the "AI codebase safety audit" checklist in
[jdpru/cover-your-ais](https://github.com/jdpru/cover-your-ais/blob/main/prompts/ai-codebase-safety-audit.md).
Prompts are not security boundaries: this skill finds hazards, the fix is always a real control.

## When it runs

| Trigger | Scope |
|---|---|
| You created or changed a workflow, AGENTS.md/CLAUDE.md/skill/hook/settings/MCP config, or a shell script | **Change review**: only the changed files, only the checks in the table below that apply to them |
| Someone asks for a safety audit of the repo or the environment | **Full audit**: all eleven checks over the repo and the session's environment |

The PostToolUse hook in `.claude/settings.json` (`scripts/post-edit-hook.py` in this skill)
reminds you after each such edit. Do the review before the commit, not after the push.

## Ground rules

1. Look first, change nothing until the findings are listed.
2. Never print a secret value. Name the variable, file and line, show at most a masked prefix.
3. Every finding: `file:line`, the hazard in one sentence, the concrete fix.
4. Change review: fix findings in your own change directly (it is your diff). Full audit: list
   in-repo fixes and wait for approval before applying them.
5. Anything outside the repository (tokens, branch protection, cloud settings, the operator's
   laptop) goes into a list of manual steps for a human; never change it yourself.
6. Rank fixes by blast radius (what is the worst one wrong command could destroy or leak?),
   top three first.
7. A risky command that has to stay gets a visible warning in its own output naming the hazard.

## Which checks apply to which change

| Changed file | Checks |
|---|---|
| `.github/workflows/*`, `action.yml` | 2, 4, 9, 11 (and 3 for inline `run:` scripts) |
| `AGENTS.md`, `CLAUDE.md`, `.cursorrules`, skills | 5, 8, 10 (and 11 if it tells agents to send, pay or deploy) |
| `.claude/settings.json`, hooks, MCP configs | 8, 10, 5 |
| `*.sh`, shell shebang, `justfile`, `Makefile` | 3, 1, 2, 4 |

## The eleven checks

**1. Tests or previews against real data.** Test, dev and preview configs (env files, compose,
CI env, fixtures) must point at a local or container database, never a shared or production host.
*Fix:* local/container DB, plus a guard in the test setup that aborts when the host is not
`localhost`/a container name.

**2. Broad credentials in reach.** Secrets readable from the shell or committed: env vars,
`~/.aws`, `~/.kube`, `gh`/`npm` tokens, keys or `.env` files in the repo, secrets echoed in
workflow logs, `secrets: inherit` where one secret would do.
*Fix:* remove committed keys and ignore the files; least-scope tokens; rotation of anything
exposed is a manual step (`secret-rotation` skill where present).

**3. Deletes that can hit the wrong target.** `rm -rf`, `find -delete`, `git clean -fdx`,
`docker … down -v` whose target comes from a variable, glob, `cd` or symlink; scripts without
`set -euo pipefail`; cleaned folders containing symlinks; tests that move `HOME`.
*Fix:* `"${VAR:?}"` for every variable in a path that gets deleted, `set -euo pipefail`,
resolve with `realpath` and check the result is inside the expected root, a `--dry-run` mode,
never `rm -rf "$DIR/"*` with an unchecked `DIR`.

**4. Infrastructure one command destroys.** Databases or volumes without deletion protection,
`terraform`/`tofu` with `-auto-approve`, stray `*.tfstate`, `--force`/`--yes` in deploy or
teardown scripts and workflows.
*Fix:* `deletion_protection`/`prevent_destroy`, no auto-approve outside a reviewed pipeline,
state in a locked remote backend, destructive steps behind a manual approval environment.

**5. Rules that exist only as prose.** A "never do X" in AGENTS.md, CLAUDE.md, a skill or
`.cursorrules` without a control that enforces it.
*Fix:* back each such rule with a real control: a `deny` rule in `.claude/settings.json`, a
hook, a read-only role or token, branch protection, a CI check. Name the control next to the rule.

**6. Branch protection and token scope.** Is `main` protected (required reviews/checks, no
force push, no deletion)? What scopes does the agent's GitHub token have?
*Fix (manual):* rulesets/branch protection, fine-grained tokens limited to the repos and
permissions needed.

**7. Synced folders.** A checkout inside iCloud, Dropbox or OneDrive; zero-byte or cloud-only
placeholder files.
*Fix:* move the repo out of the synced folder; prefer `mv` over copy-then-delete.

**8. Agent supply chain and bypasses.** MCP servers, hooks, install scripts, skills and
plugins from unknown or unpinned sources; flags like `--dangerously-skip-permissions`, `--yolo`,
`bypassPermissions`, `--no-verify`; hidden Unicode (bidi controls, zero-width characters) in
instruction files.
*Fix:* pin versions/hashes (`skills-lock.json`), remove unknown hooks, strip hidden Unicode
(`grep -nP '[\x{200B}-\x{200F}\x{202A}-\x{202E}\x{2066}-\x{2069}\x{FEFF}]'`), read-only tokens
for MCP servers.

**9. CI that runs agents on untrusted input.** A workflow where an AI agent (or any script)
processes PR titles, bodies, comments, issue text or fork code while holding secrets or write
permissions: `pull_request_target`, `issue_comment`, `workflow_run` checking out the PR head,
`${{ github.event.* }}` interpolated into `run:` (script injection).
*Fix:* top-level `permissions: {}` and per-job least privilege, no secrets in jobs that read
untrusted input, pass event data via `env:` not `${{ }}` in `run:`, pin third-party actions to a
commit SHA, gate on author association.

**10. Full-access modes and safety nets that never load.** Auto-approve or bypass modes in
settings; safety nets the agent does not actually get (an `rm` alias in `.zshrc` is invisible to
a non-interactive `bash -c`; check with `bash -c 'type rm'`).
*Fix (manual where outside the repo):* disable bypass, sandbox to the project, replace aliases
with deny rules, hooks or wrapper scripts on `PATH`.

**11. Money and messages without limits.** Tools, scripts or workflows that send e-mail, SMS,
calls, payments or paid API usage without a per-call cap, recipient allowlist or human
confirmation.
*Fix:* per-call and per-day cap, recipient allowlist (test numbers/addresses outside PROD),
explicit human confirmation, a dry-run default.

## Output

Change review: one short block in your reply (and in the PR description under a `Safety`
heading when the PR touches these files):

```
Safety review (codebase-safety): <files>
- OK: checks 3, 9 — <one line why>
- Fixed: scripts/x.sh:12 — rm -rf "$OUT/" unguarded → "${OUT:?}" + realpath check
- Manual: <step for a human, if any>
```

Full audit: a table `# | Blast radius | file:line | Hazard | Fix | in-repo/manual`, ordered by
blast radius, top three called out first, then the manual-steps list.
