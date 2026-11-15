# Shadow mode on Arztagent

- **Goal:** Schlange runs next to Mergify on `karma-works/arzt-agent` and makes the same
  decisions without acting (KONZEPT.md, feature S-SHADOW, phase 1).
- **Acceptance:** two weeks without a deviating decision, then the switchover.
- **Parity scope:** [Arztagent parity](arztagent-parity.md) accepts GitHub's last-request
  windows for base and label changes under the documented safeguards; it does not require
  a stronger atomicity guarantee than Mergify's publicly evidenced behaviour.
- **Prerequisite:** S-SHADOW is built (`schlange serve --shadow`, `schlange shadow report`,
  see AGENTS.md). Before Arztagent, it must make the same decisions in the sandbox
  (`queue-lab`, next to Mergify); the open points are in
  `.agents/skills/verify-schlange/features/S-SHADOW.md` ("confirmed on the sandbox").

## Principle: shadow through permissions, not through a flag

The shadow has its own app, "Schlange Schatten Arztagent"
([manifest](../../sandbox/apps/manifests/shadow.json)), which has **read permissions only**:
metadata, contents, issues, pull requests, checks, actions and statuses, all `read`. With it, Schlange
technically cannot merge, comment or create branches on Arztagent, even if the code has a bug
or is started without `--shadow` by mistake.

Nothing changes in the Arztagent repo: `.mergify.yml`, `pr-gate.yml`, the rulesets and Mergify
stay as they are. The risk for the running operation is zero.

## Setup

| Step | Who |
|---|---|
| 1. Register the app: `python3 -I sandbox/apps/create_app.py shadow --user karma-works --webhook-url <url>`, click "Create GitHub App" in the browser | you (one click) |
| 2. Install the app only on `karma-works/arzt-agent` (the script prints the link) | you (admin consent) |
| 3. Create a Mergify API key for Arztagent (dashboard → API keys), read permissions are enough | you |
| 4. Set up an instance on our own infrastructure (VPS): container, volume `/data`, app key as a secret, the app's webhook URL pointing to the instance | agent, after you approve the host |
| 5. Start `schlange serve --shadow --mergify-login mergify[bot]`; `schlange doctor` must be green (permissions, webhook delivery, rate limit) | agent |
| 6. Daily report `schlange shadow report`, deviations as an issue in the Schlange repo | agent |

Arztagent is a medical project. The PR data therefore lives only on our own infrastructure and
not with a third party (no smee.io in continuous operation).

## What is compared

Schlange makes its decisions and at the same time reads what Mergify actually did. The sources
are the queue PRs of `mergify[bot]`, the merges, the comments and check runs of Mergify, and
the events of the Mergify API.

| Level | Comparison |
|---|---|
| Enqueue | per PR: enqueued yes/no, which rule, which condition was missing |
| Dequeue | dequeue reason per PR |
| Commands | `@mergifyio queue`/`dequeue`: accepted or rejected (`commands_restrictions`) |
| Bisection | which PR counts as the culprit, which PRs land anyway |
| Landing | order and composition of the batches (tolerant: batches depend on timing), squash commit message |

**Limit:** In shadow mode, Schlange creates no queue PRs of its own. For the merge conditions
it takes the CI results of Mergify's queue PRs as input. This tests the decision logic, not its
own speculation. The E2E sandbox (`schlange-e2e`) tests that.

A deviation counts as fixed only when it has been reproduced as a scenario in the simulated
forge and fixed with a test (KONZEPT.md, rule 1).

## Switchover after acceptance

1. The shadow app gets write permissions (contents, pull requests, checks, workflows:
   write). GitHub asks you, as admin, for consent.
2. Constitution PR by a human: `pr-gate.yml` accepts the bot login of the Schlange app
   (today hard-coded `mergify[bot]`).
3. Ruleset bypass for the Schlange app on `main`:
   `SCHLANGE_APP_ID=<app id> bash scripts/schlange-setup.sh setup`.
   Also migrate the existing Mergify-only update lock and verify effective branch protection:
   adding an exemption to a different ruleset alone does not permit the new queue to write.
4. Pause Mergify, do not uninstall it. It stays as the fallback until Schlange has landed
   stably for a few weeks.

Use one active Schlange instance initially. For multiple instances, route webhooks to the
leader using `/readyz` (all repositories) or authenticated `/status` (per repository). A
follower returns 503 before storing a new delivery. Ingress must retry it on the leader, or
an operator must redeliver it: GitHub does not automatically retry failed webhooks. `/healthz`
does not establish leadership. See the parity contract for acceptance/demotion races and the
limits of separate data volumes.

## Intermediate step (optional, before S-SHADOW)

The read-only app can be installed earlier and only record events (webhooks plus
reconciliation). This yields real Arztagent flows that can be replayed in the simulated forge.
This creates no risk for Arztagent, because the app can only read.
