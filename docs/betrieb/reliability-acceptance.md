# Reliability fixes and acceptance boundaries

This change implements the local fixes tracked in issues #25–#28, with sandbox
preparation for #29. It also reconciles the source refresh, Arztagent fixture and
app-qualified check matching from PRs #21, #23 and #24. It does not authorize or
perform the production changes in issue #31.

| Area | Implementation and regression evidence |
|---|---|
| Shadow observer (#25) | Confirmed queue-draft membership, queue-prefix guard, terminal member lists instead of arbitrary references, closed-PR dequeue observations and consistent completed-run IDs. Fake `shadow` tests and observer unit tests cover these paths. Both app manifests and doctor require `actions: read`. |
| Ruleset doctor and cutover (#27) | Default/configured queue bases, ref include/exclude selectors, `always` fails. Fake `client` tests cover applicability and permissions. Mocked setup tests cover option parsing, backup, exclusive in-place legacy lock migration and status output. See [cutover.md](cutover.md). |
| Required checks (#26) | Numeric integration pins survive API reads, persistence, injected conditions, explanations and invariant checks. Same-name foreign/unknown apps cannot satisfy them. Ruleset webhooks reconcile immediately; required checks are loaded before a fresh snapshot can release the identity hold. Failed reads remain held. See `p2-required-checks-app-pin.yaml` and fake `e2e_p2` tests. |
| Durable inbox (#26) | The service acknowledges a signed checkbox edit, stops its worker before processing the edit, restarts on the same database without redelivery and preserves the editor identity. Repeated delivery is a duplicate. Test: `acknowledged_checkbox_edit_survives_a_crash_before_worker_processing`. This injects a worker crash; it is not an OS-level process-kill test. |
| Runtime validation (#28) | `serve` and runtime doctor use Schlange's embedded clean-room contract by default. Unknown keys, wrong types, invalid conditions, unsupported enabled options and invalid semantic ranges are rejected. Explicit external schemas remain supported; development `config validate` still requires the upstream schema. A real-binary reload test keeps the last valid config after invalid updates. |
| VPS operations (#28) | Compose, TLS readiness routing, restricted egress, file secrets, monitoring, online SQLite backups, timers and restore procedure are in [vps.md](vps.md). Local parser/script checks do not replace deployment verification on the target host. |

## External acceptance still required

- Record and review real Mergify baselines for S14–S19, recheck provisional S11
  and the S13/S16/S17 conflict-reason distinction. Never infer a baseline from a
  fake or timed-out run. The sandbox workflow supports active and shadow runs;
  see [sandbox.md](sandbox.md).
- Confirm real queue-draft member formats and dequeue-code output. The observer
  recognizes the concrete terminal `(#n, #m)` member list and an explicit body
  member section; unknown forms are not arbitrary-number scraped. A check-only
  queue entry without a draft can remain unobserved. Literal `PR_CLOSED` is
  preserved as a discrepancy against the engine's current assumed
  `PR_DEQUEUED`; it is not silently normalized. Repeated-entry shadow history
  comparison remains incomplete.
- Existing shadow facts are historical evidence. Start a new acceptance data
  directory after deploying observer fixes; do not silently erase old facts.
- Obtain fresh independent critic verdicts before promoting any feature to
  `conformant`. No feature is promoted by these local fixes alone.
- Run 14 consecutive days of read-only shadow acceptance and obtain human
  approval for the guarded single-writer production cutover (#31).
- The optional MCP server (#30) remains separate work; existing JSON CLI and
  authenticated admin endpoints provide the current agent interface.
