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
- `schema`: built (reports the embedded clean-room runtime contract as OK, or checks an explicitly supplied upstream schema).
- `matrix`: built (support matrix embedded). Counted honestly: `N conformant, M partial (some sub-features missing), K not built`; a partial feature is not counted as supported.
- `configuration`: built, one check for both sources. Local: `--config <file>`, or the configuration file the search finds in the working directory (`.mergify.yml` and the other names; two files FAIL). GitHub, with app credentials: each repository's configuration file from the default branch (the six documented names; two files FAIL). Both go through the validator `serve` uses (stages 1–4, stage 5 as warnings): FAIL when missing on the default branch or invalid (`serve` then keeps the last valid one), WARN for features Schlange does not support yet, OK otherwise. With both sources the worse status wins and both details are shown. Without an explicit upstream schema both sources use the embedded clean-room structural validator (unknown keys, bad types, invalid conditions and explicitly enabled unsupported options FAIL); without a local file and offline the check is SKIP. Test: `cargo test -p schlange --test cli doctor_validates_the_configuration_and_counts_partial_apart`.
- `github-app`: built. With `SCHLANGE_GITHUB_APP_ID` and `SCHLANGE_GITHUB_PRIVATE_KEY` (or flags): `GET /app` with the app JWT.
- `app-permissions`: built. The installation's permissions against the minimum (FAIL when one is missing, WARN for extra ones and for the optional `workflows: write`, W-22).
- `ruleset`: built. Only active branch rulesets matching a queue base require the app as an `exempt` bypass actor. Queue bases come from base conditions in valid configured queue rules over existing branches, falling back to the repository default branch when configuration is unavailable or invalid. Include selectors (`~DEFAULT_BRANCH`, `~ALL`, full-ref globs) are reduced by exclusions. Missing bypass or mode `always` is FAIL with setup remediation; an unreadable bypass list is WARN. Missing branch selectors are treated conservatively as applying to all queue bases. The detail says why and what it costs: a batch lands as squash commits whose checks ran on the queue's draft, which a pull-request ruleset refuses without a bypass; a bypass skips every rule of that ruleset for the app's merges, so it belongs to the app only and the queue's `merge_conditions` must be as strict as the protection.
- `webhooks`: built. The app's last deliveries: WARN when one failed or the newest is older than a day.
- `rate-limit`: built. `GET /rate_limit`: WARN below 10 % left.
- `offline`: built. `--offline` skips the GitHub checks; the container health check uses it so that it does not spend rate limit every 30 seconds.

## How to get to it (user POV)

- `schlange doctor`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Binary.** Run `target/release/schlange doctor`. Exit `0`; lines `OK version`, `OK support-matrix` (with conformant/partial/not built counts), `OK schema` (or `WARN schema` without `.research/`), `SKIP` for the six GitHub checks when no app is configured.
- **GitHub checks.** Run `cargo test -p schlange --test serve` (the binary against the fake GitHub, `doctor --json` with the app's environment: `github-app`, `webhooks`, `rate-limit` OK, `configuration` not FAIL) and `cargo test -p schlange-github-fake --test client doctor` (missing permission → FAIL, extra permission named, ruleset without bypass → FAIL with the risk explained, `always` → FAIL with setup remediation, unrelated/excluded branch rulesets → OK, configured release queue base and non-main default branch respected, no `.mergify.yml` → FAIL, a rule without `conditions` → FAIL "invalid", no schema → FAIL, unreachable app → FAIL).
- **Container.** Run `docker run --rm --read-only --cap-drop ALL schlange:verify doctor`. Exit `0`; `WARN schema` because the image holds no Mergify file.
- **Proof.** `schlange doctor --json` gives `ok: true` and one entry per check.

## Gotchas

- The container has no schema; that is a WARN, not a FAIL.
- Against the real GitHub the checks are unconfirmed: whether `bypass_actors` and `/app/hook/deliveries` are readable with the minimal permissions needs a sandbox.
