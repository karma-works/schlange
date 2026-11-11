# Migrate the main update lock from Mergify

This is an operator procedure after the reviewed sandbox run and fourteen-day read-only
acceptance. It changes GitHub access controls. Do not run it as part of development tests,
and do not start an active Schlange writer while Mergify is still writing.

`scripts/schlange-setup.sh` manages repositories whose default branch is `main`. It requires
`bash`, `gh`, `jq`, an authenticated repository administrator, and the numeric GitHub App IDs
from the app settings. Client IDs are not app IDs. Commands accept `--yes` before or after the
subcommand; without it, mutations require confirmation. Unknown arguments exit with code 64.

1. Pause/drain Mergify and stop the active Schlange instance. Keep the read-only shadow app
   separate from the active app. Verify no queue merge is in flight.
2. Complete the human-owned constitution PR and active-app permission changes described in
   issue #31. In particular, grant `workflows: write` if queued PRs change workflow files.
3. Inspect access and save the output in the change record:

   ```bash
   export SCHLANGE_REPO=owner/repository
   export SCHLANGE_APP_ID=123456
   export SCHLANGE_APP_SLUG=your-active-schlange-app
   bash scripts/schlange-setup.sh status
   ```

   Status lists every Integration bypass actor and its mode for main rulesets, plus all
   classic push restrictions. The slug is required when classic app push restrictions exist;
   the script verifies that its public app metadata has the selected numeric ID.
4. Migrate the existing lock in place, supplying Mergify's numeric app ID:

   ```bash
   bash scripts/schlange-setup.sh cutover-from-mergify --revoke-app 654321
   ```

   The command requires exactly one update lock covering main. It accepts the old name
   `Merge nur über Mergify` and the already-migrated name `Merge only via Schlange`.
   Missing, unknown or competing update locks cause an error before mutation. The command
   verifies the old ID against the public `mergify` app metadata.

   Before writing, it backs up workflows, complete rulesets and classic branch protection
   under `.cache/schlange-setup/<UTC timestamp>-<unique suffix>/` (override with
   `SCHLANGE_BACKUP`). Keep this directory securely with the change record.

   The first write replaces the exclusive lock's bypass list in one API request: only
   Schlange as Integration with `exempt`, and repository administrators with `pull_request`.
   Other rules and branch conditions remain in place; the ruleset ID stays the same. Other
   Integration/user/team bypasses on that exclusive lock are intentionally removed.
   Subsequent writes remove Mergify's Integration bypass from main rulesets and its classic
   app push exception, then grant Schlange the needed exemptions. GitHub has no transaction
   spanning all rulesets; both queues must remain paused until every step finishes. A failed
   intermediate operation may temporarily block both queues. Inspect status and retry the
   same command; the migration is idempotent. Never restore Mergify access while Schlange is
   running.
5. Run `status` again, verify the main update lock names only Schlange and administrators,
   and run active `schlange doctor`. Resolve every FAIL and unexpected WARN before starting
   one active Schlange instance. Verify `/readyz`, then smoke-test ordinary and human-queued
   constitution PRs and their audit proofs. Keep Mergify installed and paused as fallback.

`lock-main` creates a new lock only when no main update lock exists; it refuses to create a
competing lock next to Mergify's legacy lock. It also backs up before mutation. `setup` is the
ordinary app-exemption/label helper, not a substitute for the explicit cutover command: it
does not revoke the old writer. Ruleset applicability uses ref-name includes minus excludes,
including default/all tokens and path globs.

For rollback, pause/stop Schlange first, inspect the saved ruleset and classic-protection
JSON, restore the reviewed access policy through an administrator, verify a single queue
writer, and only then resume Mergify. Do not blindly replay all snapshots: other administrators
may have changed protections since the backup. Keep failed GitHub webhook deliveries for
explicit redelivery after readiness recovers.

Offline regression checks (mocked `gh`, no GitHub calls):

```bash
bash -n scripts/schlange-setup.sh
python3 -m unittest discover -s scripts/tests -v
```

This procedure does not establish live ruleset, sandbox, or fourteen-day acceptance evidence.
