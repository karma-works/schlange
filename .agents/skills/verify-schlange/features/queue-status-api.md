---
id: queue-status-api
title: "Queue status through API and CLI"
priority: P2
arztagent: false
status: partial
inventory:
  - "api:GET /repos/{owner}/{repository}/merge-queue/status"
  - "api:GET /repos/{owner}/{repository}/merge-queue/branches"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pull/{pr_number}"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pull/{pr_number}/dequeue-impact"
  - "api:POST /repos/{owner}/{repository}/merge-queue/pull/{pr_number}/dequeue"
  - "api:GET /repos/{owner}/{repository}/queues"
  - "api:GET /repos/{owner}/{repository}/queues/branches"
  - "api:GET /repos/{owner}/{repository}/queues/configuration"
  - "cli:mergify queue status"
  - "cli:mergify queue show"
unsupported:
  - "api:GET /repos/{owner}/{repository}/merge-queue/branches"
  - "api:GET /repos/{owner}/{repository}/merge-queue/pull/*"
  - "api:POST /repos/{owner}/{repository}/merge-queue/pull/*"
  - "api:GET /repos/{owner}/{repository}/queues*"
  - "cli:mergify queue show"
problems:
  - "MQ-43"
  - "W-13"
  - "W-25"
source: "https://docs.mergify.com/api-usage"
---

# Queue status through API and CLI

Read the state of the queue, of single pull requests and the effect of a dequeue; dequeue a pull
request through the API.

## Sub-features

- `status`: built. `schlange_core::status::queue_status` gives the documented shape in our own words: `mode`, `batches` (id = car id, name = queue branch, `parent_ids`, `status.code` mapped onto the documented codes: `preparing`, `running`, `waiting_for_merge`, `waiting_for_previous_batches`, `frozen`, `failed`, `bisecting`, `waiting_for_requeue`, `started_at`, `scopes`, `queue_rule_name`, `checks_summary`, `pull_requests`, `queue_pull_request_number`), `waiting_pull_requests` (number, title, `queued_at`, `priority_alias`, `priority_rule_name`, `scopes`, `all_scopes`, `is_frozen`), `pause`; plus `freezes`, `halted` and a `held` text per pull request. `schlange serve` serves it at `GET /repos/{owner}/{repo}/merge-queue/status` behind the admin token (401 without, 404 without a configured token); `schlange queue status [--json]` prints it.
- `pull-request`, `dequeue-impact`, `dequeue`, `configuration`, `branches`: missing.
- `estimates`: missing (`estimated_merge_at` and the like are not reported).

## How to get to it (user POV)

- `schlange queue status --url http://serve:8080 --repo owner/name` with `SCHLANGE_ADMIN_TOKEN` (or `--admin-token-file`); exit 0, 1 when serve refused, 3 when it cannot be reached.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- `cargo test -p schlange-github-fake --test e2e_p2 the_queue_api_pauses_and_resumes_and_shows_the_status` (waiting pull request, a batch with its draft number, the pause).
- `cargo test -p schlange --test serve` (the CLI against the real binary).

## Gotchas

- Schlange output is `--json` with a stable schema (S-CLI): `schlange queue status --json` wraps the response as `{status, response}`.
- The listener has no TLS; put a TLS proxy in front before using the API across a network.
