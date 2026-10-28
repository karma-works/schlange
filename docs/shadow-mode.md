# Shadow mode: set it up in a repository

This guide sets up Schlange in shadow mode next to the merge queue a repository already uses
(Mergify). It is written so that an agent can follow it step by step in another project. The
Arztagent runbook ([betrieb/schattenbetrieb.md](betrieb/schattenbetrieb.md)) applies it to one
repository and adds the cut-over plan.

## What shadow mode is

`schlange serve --shadow` receives the same webhooks, reads the same pull requests and
configuration, and makes every decision it would make as the queue: queue, batch, test, land,
dequeue. It sends none of them to GitHub. Instead it watches what the active queue does and
stores, per pull request, whether the two agree. `schlange shadow report` shows the result.

Three things keep it from writing:

1. **Permissions.** The GitHub App has read access only. GitHub refuses any write, whatever
   Schlange's code does.
2. **Write gate.** In shadow mode the HTTP client refuses every request but a `GET` before it
   is sent (installation tokens excepted).
3. **Read-only tokens.** `serve --shadow` asks GitHub for installation tokens with read
   permissions only, even if the app could write.

Nothing in the repository changes: no workflow, no ruleset, no configuration file.

## Prerequisites

| What | Check |
|---|---|
| The repository uses Mergify with a `.mergify.yml` on its default branch | Mergify's check runs appear on pull requests |
| You can install a GitHub App on the repository (owner or admin) | – |
| A host that runs a container permanently, with outbound HTTPS to `api.github.com` | `curl -sI https://api.github.com` |
| An HTTPS URL that reaches the host's port 8080 for webhooks (section 6) | – |
| Docker (or the static binary) on the host | `docker version` |
| A checkout of Schlange to build the image and fetch Mergify's schema | `cargo`, `git` |

## 1. Validate the configuration

On a machine with a Schlange checkout:

```bash
tools/fetch-mergify-sources                         # the pinned schema into .research/mergify/
cargo build --release -p schlange
target/release/schlange config validate --strict /path/to/the/repository/.mergify.yml
```

| Exit | Meaning | Next |
|---|---|---|
| 0 | valid, everything supported | go on |
| 2 | valid for Mergify, but uses something Schlange does not support | the output names each path and feature; shadow mode will report those decisions as disagreements. Decide whether that is acceptable before going on |
| 1 | invalid | fix the configuration first; Mergify is probably rejecting it too |

## 2. Create the GitHub App

The app must have exactly these permissions and events. More is a warning in
`schlange doctor --shadow`; a write permission defeats the first safeguard above.

| Permission | Level | Why |
|---|---|---|
| Metadata | Read | mandatory; rulesets |
| Contents | Read | `.mergify.yml`, commits, compare, branches |
| Pull requests | Read | pull requests, files, comments (commands), labels, reviews |
| Checks | Read | check runs, including Mergify's |
| Commit statuses | Read | statuses used as conditions |

Webhook events: `check_run`, `check_suite`, `issue_comment`, `pull_request`,
`pull_request_review`, `push`, `status`.

**With the script (recommended):** from a Schlange checkout, on a machine with a browser:

```bash
python3 -I sandbox/apps/create_app.py shadow --org <owner>  --webhook-url https://<host>/webhook --name "Schlange Shadow <owner>"
# for a personal account: --user <login> instead of --org <owner>
```

It opens GitHub with the manifest ([sandbox/apps/manifests/shadow.json](../sandbox/apps/manifests/shadow.json));
a person clicks "Create GitHub App". The script stores the app's metadata, private key and
webhook secret under `~/.config/schlange-sandbox/apps/shadow.*` (mode 0600). App names are
unique across GitHub; pick another `--name` if GitHub says it is taken.

**By hand:** GitHub → Settings → Developer settings → GitHub Apps → New GitHub App. Set the
permissions and events from the tables, a webhook URL and a random webhook secret of at least
16 bytes (`openssl rand -hex 32`), "Only on this account". After creating it, generate a private
key and note the App ID.

## 3. Install it on the repository

App page → Install App → the owner → **Only select repositories** → the repository. A person
has to confirm this in the browser. Install it on nothing else.

## 4. Prepare the host

```bash
docker build -t schlange:shadow .                   # in the Schlange checkout; ~11 MB image
```

Copy the image (or build it on the host) and create, on the host:

```
/srv/schlange-shadow/
├── data/            # SQLite database; owned by uid 65532
├── secrets/
│   ├── app.pem              # the app's private key           (0400, uid 65532)
│   ├── webhook-secret       # the webhook secret               (0400, uid 65532)
│   └── admin-token          # random, for the admin API        (0400, uid 65532)
└── schema.json      # .research/mergify/schema.json from step 1
```

```bash
sudo install -d -o 65532 -g 65532 /srv/schlange-shadow/data
openssl rand -hex 32 | sudo tee /srv/schlange-shadow/secrets/admin-token >/dev/null
sudo chown -R 65532:65532 /srv/schlange-shadow/secrets && sudo chmod 0400 /srv/schlange-shadow/secrets/*
```

`serve` does not start without the schema: it validates the configuration like
`config validate` and never applies an invalid one.

## 5. Check, then start

The same environment serves `doctor` and `serve`:

```bash
cat > /srv/schlange-shadow/env <<EOF
SCHLANGE_GITHUB_APP_ID=<app id>
SCHLANGE_GITHUB_PRIVATE_KEY=/run/secrets/app.pem
SCHLANGE_REPOS=<owner>/<repository>
SCHLANGE_WEBHOOK_SECRET_FILE=/run/secrets/webhook-secret
SCHLANGE_ADMIN_TOKEN_FILE=/run/secrets/admin-token
SCHLANGE_SCHEMA=/schema.json
SCHLANGE_SHADOW=1
SCHLANGE_MERGIFY_LOGIN=mergify[bot]
SCHLANGE_MERGIFY_APP=mergify
EOF

run() {
  docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
    --env-file /srv/schlange-shadow/env \
    -v /srv/schlange-shadow/secrets:/run/secrets:ro \
    -v /srv/schlange-shadow/schema.json:/schema.json:ro \
    -v /srv/schlange-shadow/data:/data "$@"
}
run schlange:shadow doctor --shadow
```

Expected: no FAIL line; in particular

| Check | Expected |
|---|---|
| `github-app` | OK, the app's slug |
| `app-permissions` | OK "read-only: exactly what shadow mode needs" (WARN means more than read access: remove it) |
| `ruleset-exception` | SKIP "shadow mode never lands" |
| `configuration` | OK, the repository's `.mergify.yml` is valid |
| `webhooks` | WARN until the first delivery arrived; OK afterwards |
| `rate-limit` | OK |

Then start it for good:

```bash
docker run -d --name schlange-shadow --restart unless-stopped \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --env-file /srv/schlange-shadow/env \
  -v /srv/schlange-shadow/secrets:/run/secrets:ro \
  -v /srv/schlange-shadow/schema.json:/schema.json:ro \
  -v /srv/schlange-shadow/data:/data \
  -p 127.0.0.1:8080:8080 \
  schlange:shadow serve
docker logs -f schlange-shadow      # start-up reconciliation, then one line per decision
```

The port is bound to localhost on purpose: a TLS terminator in front of it (section 6) is
the only way in.

## 6. Deliver webhooks

GitHub must reach `https://<host>/webhook`. Schlange checks every delivery's signature with
the webhook secret and ignores the rest.

- **Reverse proxy with TLS (recommended).** For example Caddy with a DNS name pointing at the
  host; it obtains the certificate itself:

  ```
  schlange.example.org {
      reverse_proxy /webhook 127.0.0.1:8080
  }
  ```

  Expose only `/webhook`. Keep the admin API (`/status`, `/repos/…`) on localhost or behind
  another authentication.
- **A relay** such as smee.io forwards deliveries through a public channel. Only for public
  repositories: anyone who knows the channel URL can read the payloads.
- **Without webhooks** Schlange still reconciles every 300 seconds (`--reconcile-every`), so it
  catches up, but its decisions lag and the comparison of timing-dependent things (batches)
  gets noisier.

Lost or delayed deliveries do no harm beyond that: reconciliation is the truth.

## 7. Confirm it does not write

```bash
curl -s -H "Authorization: Bearer $(sudo cat /srv/schlange-shadow/secrets/admin-token)" http://127.0.0.1:8080/status
```

`role` is `shadow`; `shadow_suppressed` grows as Schlange decides things it does not send. On
GitHub, the app has no write permission (step 2), and the repository's pull requests show no
comment, label, check run or branch from it.

## 8. Read the report

```bash
docker exec schlange-shadow /usr/local/bin/schlange shadow report --repo <owner>/<repository> \
  --url http://127.0.0.1:8080 --admin-token-file /run/secrets/admin-token
# or offline from the database, also while serve is stopped:
docker run --rm -v /srv/schlange-shadow/data:/data schlange:shadow shadow report --repo <owner>/<repository> --data-dir /data
```

Add `--json` for machines. Exit 0 without disagreements, 1 with.

Per pull request it compares: queued or not, landed or not, landing order (among the pull
requests both landed), the batch it landed in, and the dequeue reason.

| Verdict | Meaning |
|---|---|
| `agree` | same outcome |
| `disagree` | different outcome; the report names the field |
| `inconclusive` | Mergify merged before Schlange decided (usually right after a restart) |
| `pending` | not finished yet |

## 9. Work the differences

Every `disagree` is a finding, not noise to tune away:

1. Reproduce it as a scenario in Schlange's simulated forge (`scenarios/`) or in the GitHub
   sandbox ([.agents/skills/sandbox-e2e](../.agents/skills/sandbox-e2e/SKILL.md)).
2. Decide who is right from Mergify's documentation and observed behaviour.
3. Fix Schlange with a test, or record a deliberate difference in the feature file.
4. Deploy the fix and keep comparing.

**Acceptance:** two weeks of normal traffic without a `disagree`. Batches depend on timing; a
difference only in batch composition with the same landing result is worth a look, not a
reason to restart the clock.

## 10. Switch, and the way back

Only after acceptance:

1. Give the app the permissions of an acting queue (contents, pull requests and checks:
   write; workflows: write if pull requests change `.github/workflows/`). GitHub asks an
   owner to approve.
2. Add the app as a bypass actor of the rulesets on the default branch (Mergify asks for the
   same).
3. Update workflows that recognise the queue's bot by login (Arztagent's `pr-gate.yml` checks
   `mergify[bot]`) and the queue branch prefix if you change it.
4. Restart without `SCHLANGE_SHADOW`, run `schlange doctor` (without `--shadow`) until it has no
   FAIL, then pause Mergify.
5. Keep Mergify installed and paused for a few weeks. To go back: stop Schlange, unpause
   Mergify; nothing else needs to change.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `HTTP 403: Resource not accessible by integration` in the log | a permission from step 2 is missing; `doctor --shadow` names it |
| `doctor` says `app-permissions` FAIL with `--shadow` missing | you ran `doctor` without `--shadow`; the acting queue needs write access |
| `configuration not read` / invalid | the schema is missing or older than the configuration; fetch it again (step 1) |
| no decisions at all | no webhooks and the first reconciliation has not run yet; check `docker logs` and `doctor`'s `webhooks` line |
| every pull request `disagree` on "queued" | Mergify's bot login or app slug differs; set `SCHLANGE_MERGIFY_LOGIN` and `SCHLANGE_MERGIFY_APP` |
| `inconclusive` piling up | Schlange restarted often or reconciles slowly; check webhook delivery |

## Data protection

Schlange stores pull request metadata (numbers, authors, labels, check results, comments that
are commands) in its SQLite database on the host. Run it on infrastructure you would also trust
with the repository's code, and list the host's provider where your project lists processors.
