# GitHub sandbox

- **Purpose:** E2E and differential tests against real GitHub (KONZEPT.md, section 9). Useful
  even before the first Schlange code: the scenarios record how Mergify behaves. That is the
  observed behaviour for the clean-room rebuild and the reference value for the differential
  test.
- **Code:** [sandbox/](../../sandbox/)

## Layout

Organisation `schlange-sandbox` (free plan), owner `karma-works`. The repos are public,
because the free plan offers rulesets only for public repos. Actions minutes are free there,
and so is Mergify for open source. The content is synthetic.

| Repo | Queue | Schlange | Purpose |
|---|---|---|---|
| `queue-lab` | Mergify | shadow (read only) | differential test: same PRs, compare decisions |
| `schlange-e2e` | Schlange | active | E2E: queue PRs, rulesets, merges |

Both repos get the same fixture ([sandbox/fixture/](../../sandbox/fixture/)):

- `ci.yml` with the required result `ci-ok`, `light` on PRs and `full` on `mergify/merge-queue/*`.
- `pr-gate.yml` with the check `Risk class and data protection` (it checks the Datenschutz
  section). The queue bot login is in the repo variable `QUEUE_BOT_LOGIN` (the line that
  changes at the switchover).
- `ai-review.yml` with the check run `KI-Review` via `pull_request_target`, without a model.
  Red if the PR description contains "KI-Review: rot".
- Arztagent's `.mergify.yml`, only the logins are replaced.
- `package.json` and `dependabot.yml` with deliberately old dependencies. Dependabot opens real
  patch, minor and major PRs, plus an update of `actions/checkout` that changes a constitution
  path.
- Ruleset "main geschützt": PR required, checks `ci-ok` and `Risk class and data protection`,
  bypass for the queue app (Mergify or Schlange) and for admins.

The suite is controlled by flag files ([ci/run.sh](../../sandbox/fixture/ci/run.sh)):
`fail`, `fail-light`, `fail-full`, `conflicts-with <pr>`, `flaky <percent>`, `sleep`,
`sleep-full`. Only flags that the PR or the batch itself changes count. Flags that are already
on `main` no longer have any effect.

## Identities

There is no machine user. All bots are GitHub Apps: they have their own logins and narrowly
scoped permissions, and password and 2FA are not needed.

| Identity | Role |
|---|---|
| App "Schlange Sandbox Queue" | Schlange as the queue in `schlange-e2e`, shadow in `queue-lab` |
| App "Schlange Sandbox Driver" | test actor: opens PRs, pushes, comments. At the same time the sender that is *not* allowed for `commands_restrictions` |
| Mergify | reference in `queue-lab` |
| `karma-works` (gh login) | the allowed sender for `@mergifyio queue`; moves `main` through the admin bypass |

App keys and webhook secrets live only locally under `~/.config/schlange-sandbox/apps/`
(directory 0700, files 0600).

## Setup

### What only a human can do (about 15 minutes)

1. **Create the organisation:** <https://github.com/account/organizations/new?plan=free>,
   name `schlange-sandbox`. This is not possible through the API.
2. **Register two apps:** `bash sandbox/setup.sh apps` opens the browser for each app.
   One click on "Create GitHub App" is enough there. If the name is already taken, you can
   change it there.
3. **Install both apps:** the script prints the links. Choose "Only select repositories"
   there, then `queue-lab` and `schlange-e2e`.
4. **Install Mergify**, only for `queue-lab`:
   <https://github.com/apps/mergify/installations/new>. Then, in the Mergify dashboard for
   `queue-lab`, switch on the products **Merge Queue** and **Workflow Automation**. Without the
   second one, Mergify does not evaluate the `pull_request_rules`, and nothing is enqueued
   automatically. PRs opened before switching it on need a `@mergifyio refresh`.
5. **Mergify API key:** create it in the Mergify dashboard and store it at
   `~/.config/schlange-sandbox/mergify-api-key` (chmod 600). It is needed for the differential
   comparison.
6. **Fine-grained token** for the "human" in the driver:
   <https://github.com/settings/personal-access-tokens/new>, resource owner `schlange-sandbox`,
   all repos, Contents/Pull requests/Issues: write, Administration: read. Store it at
   `~/.config/schlange-sandbox/human-token` (chmod 600). It sees only the sandbox.

### What the script does

```bash
bash sandbox/setup.sh check      # state, changes nothing
bash sandbox/setup.sh all        # smee channel, apps, repos (idempotent)
```

With `--dry-run` the script only shows what it would do. The order, if you want to do
everything step by step: create the organisation → `setup.sh smee` → `setup.sh apps` (clicks,
then install) → `setup.sh repos` → install Mergify → `setup.sh check`.

`setup.sh repos` creates the repos and writes the fixture. It also sets the merge methods, the
label `halt`, `QUEUE_BOT_LOGIN` and the ruleset. `setup.sh fixture` rewrites the fixture on
`main`; this resets the content.

Webhooks: `setup.sh smee` creates a channel on smee.io. The queue app sends its webhooks
there, and locally `npx smee-client -u <channel> -t http://127.0.0.1:8080/webhook` forwards
them. For continuous operation the app's webhook URL points to the Schlange instance on the
VPS.

## Scenarios

```bash
python3 -I sandbox/driver/sandbox.py run sandbox/scenarios/s03-red-batch.json --repo queue-lab
python3 -I sandbox/driver/sandbox.py reset --repo queue-lab   # close open scenario PRs
```

| ID | Content | Catalogue |
|---|---|---|
| S01 | one PR, all green | MQ-18, MQ-20, MQ-42 |
| S02 | three PRs as one batch | MQ-26, MQ-20 |
| S03 | red batch, bisection | MQ-21, MQ-23, MQ-45 |
| S04 | green on their own, red together | MQ-01, MQ-21 |
| S05 | label `halt` while queued | MQ-11, MQ-12, MQ-45 |
| S06 | push while queued | MQ-07, MQ-14 |
| S07 | draft, ready, draft again | MQ-09 |
| S08 | constitution path, `@mergifyio queue` from an allowed and a not allowed sender | MQ-50, MQ-52 |
| S09 | red quick check, missing Datenschutz section, red KI-Review | MQ-16, MQ-37 |
| S10 | `main` moves, text conflict with `main` | MQ-06, MQ-27, MQ-47 |
| S11 | `@mergifyio dequeue` and `queue` again | MQ-45, MQ-50 |

Every run writes `.sandbox-runs/<run>/` (not checked in):

- `scenario.json`
- `run.json` (PR numbers)
- `trace.json`: timeline per PR, bot comments, checks, queue PRs with commits, commits on `main` with tree SHA
- `summary.json`: short version

The recordings contain texts of Mergify comments and therefore stay local (clean room,
KONZEPT.md section 2). Only findings in our own words go into the repo, as gotchas in the
feature files.

A scenario takes 5–40 minutes, depending on the waiting times. Mergify waits up to 5 minutes
for a full batch (`batch_max_wait_time`).

## Automatic (GitHub Actions)

The workflow [sandbox.yml](../../.github/workflows/sandbox.yml) in the Schlange repo replays
the scenarios. An agent starts it like this:

```bash
gh workflow run sandbox.yml -f repo=queue-lab -f scenarios="s01-single-pr s03-red-batch"
gh run watch "$(gh run list -w sandbox.yml -L1 --json databaseId --jq '.[0].databaseId')"
```

- **Secrets:** `bash sandbox/ci/upload-secrets.sh` uploads app keys, webhook secret, sandbox
  token and Mergify key as Actions secrets (`--list` shows the state). In the runner,
  `sandbox/ci/write-config.sh` rebuilds `~/.config/schlange-sandbox` from them. All secrets
  work only in `schlange-sandbox`.
- **Lock:** one run per sandbox repo (`concurrency`). In addition, the driver waits until no
  scenario or queue PRs are open any more, so that local runs and CI runs do not mix.
- **Baselines:** `sandbox/baselines/<scenario>.json` in our own `outcome.json` format, without
  Mergify texts. A reviewed Mergify run becomes a baseline by copying its `outcome.json`
  there. After that, every run compares: on `queue-lab`, whether Mergify has changed; on
  `schlange-e2e`, whether Schlange behaves like Mergify (`sandbox.py compare`).
- **Schedule:** Sundays, all scenarios on `queue-lab` (about 3–4 hours; free in a public
  repo).
- **Result:** table in the job summary. As an artifact (14 days) only `outcome.json`,
  `run.json` and `scenario.json`; `trace.json` and `summary.json` contain Mergify texts and
  stay in the runner. For the full recording, run the scenario locally.
- **Security:** the workflow starts only through `workflow_dispatch` (write access needed) and
  `schedule`, never through PRs from forks. The secrets work only in `schlange-sandbox`.
- **`schlange-e2e` in the runner:** the job builds the release binary and starts
  `sandbox/ci/start-schlange.sh`: `schlange serve` with the queue app against
  `schlange-sandbox/schlange-e2e`, no webhooks (no forwarder runs next to the app key): reconciliation polls every 10 s. A wait that times out fails the run on this repo
  (`SANDBOX_FAIL_ON_TIMEOUT`). The job log gets only problem lines of the serve log (it mentions
  pull request titles).
- **Mergify must not be installed on `schlange-e2e`.** If the Mergify app is installed on all
  repositories of the organisation, it posts the check run `Rule: … (queue)` with the conclusion
  `action_required` ("Merge Queue needs to be activated"). `#check-failure = 0` counts it as a
  failure (as it does for Mergify itself), so nothing is queued and every `schlange-e2e`
  scenario times out. Fix by hand: Mergify installation, "Only select repositories",
  `queue-lab` only.

## Results of the first full comparison (2026-10-28)

All eleven scenarios ran on `queue-lab` (Mergify, run 40665292551) and on `schlange-e2e`
(Schlange, run 40665289089; `s03` and `s10` again in run 40699830249 after the driver's
`quiet` wait learned that an open queue pull request or a moving `main` is activity).

- Baselines in `sandbox/baselines/` exist for `s01 s02 s03 s04 s06 s08 s09 s10`. Against them,
  `sandbox.py compare` says SAME for all eight on Schlange (the first `s03`/`s10` run said
  DEVIATION only because the old `quiet` wait ended while Schlange still tested batches).
- `s05` (label `halt` removed), `s07` (draft, ready again) and `s11` (`dequeue`, then `queue`):
  the Mergify run waited 40 minutes in each and the pull request stayed open after
  `PR_DEQUEUED` / `PR_MANUALLY_DEQUEUED`; Mergify did not queue it again. Schlange queues
  the pull request again at once and merges it. No baseline is committed for these three,
  because the Mergify run timed out. A human has to decide whether Mergify's behaviour (no
  automatic re-queue after a condition-based or manual dequeue, within 40 minutes) is the
  one Arztagent relies on; Schlange then needs a matching hold. Schlange's side of the
  outcome does not report `queued`, `enter_count` and `dequeue_codes` yet, so `compare` checks
  only the end state and the landing order.
