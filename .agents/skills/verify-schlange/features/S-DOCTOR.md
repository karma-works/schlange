---
id: S-DOCTOR
title: "`schlange doctor`"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-13"
  - "MQ-29"
  - "W-22"
source: "KONZEPT.md, section 8"
---

# `schlange doctor`

Checks version, configuration, app permissions, ruleset exception, webhook delivery and rate
limit; serves as the container's health check.

## Sub-features

- `version`: built.
- `schema`: built (reports whether a schema is found).
- `matrix`: built (support matrix embedded). Counts conformant, partial and missing features apart; `partial` is not counted as supported.
- `configuration`: built. Validates `--config <file>` or the configuration file the search finds in the working directory (`.mergify.yml` and the other names): FAIL when invalid (or more than one file), WARN when valid but using something Schlange does not support, OK otherwise, SKIP without a file; with the schema when one is found, else stages 1, 3 and 4. Test: `cargo test -p schlange --test cli doctor_validates_the_configuration_and_counts_partial_apart`.
- `github-app`: built. With `SCHLANGE_GITHUB_APP_ID` and `SCHLANGE_GITHUB_PRIVATE_KEY` (or flags): `GET /app` with the app JWT.
- `app-permissions`: built. The installation's permissions against the minimum (FAIL when one is missing, WARN for extra ones and for the optional `workflows: write`, W-22).
- `ruleset`: built. Every active branch ruleset must list the app as a bypass actor (FAIL when not, WARN when the bypass list is not visible).
- `webhooks`: built. The app's last deliveries: WARN when one failed or the newest is older than a day.
- `rate-limit`: built. `GET /rate_limit`: WARN below 10 % left.
- `offline`: built. `--offline` skips the GitHub checks; the container health check uses it so that it does not spend rate limit every 30 seconds.

## How to get to it (user POV)

- `schlange doctor`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Binary.** Run `target/release/schlange doctor`. Exit `0`; lines `OK version`, `OK support-matrix`, `OK schema` (or `WARN schema` without `.research/`), `SKIP` for the five GitHub checks when no app is configured.
- **GitHub checks.** Run `cargo test -p schlange --test serve` (the binary against the fake GitHub, `doctor --json` with the app's environment: `github-app`, `webhooks`, `rate-limit` OK) and `cargo test -p schlange-github-fake --test client doctor` (missing permission → FAIL, extra permission named, ruleset without bypass → FAIL, unreachable app → FAIL).
- **Container.** Run `docker run --rm --read-only --cap-drop ALL schlange:verify doctor`. Exit `0`; `WARN schema` because the image holds no Mergify file.
- **Proof.** `schlange doctor --json` gives `ok: true` and one entry per check.

## Gotchas

- The container has no schema; that is a WARN, not a FAIL.
- Against the real GitHub the checks are unconfirmed: whether `bypass_actors` and `/app/hook/deliveries` are readable with the minimal permissions needs a sandbox.
