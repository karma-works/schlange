//! `schlange doctor` checks against GitHub, when app credentials are configured: the app is
//! reachable, the installation has the permissions Schlange needs (and not more, W-22), the
//! rulesets on the repositories let the app bypass them, each repository's `.mergify.yml` on
//! the default branch passes the validator (the same pipeline `serve` applies), webhook
//! deliveries are recent and succeed, and the rate limit has room.
//!
//! **Why the ruleset bypass.** A batch lands as several squash commits on the base. A ruleset
//! (or classic protection) that requires pull requests with passing checks refuses a merge
//! whose checks ran on the queue's draft, not on the pull request at its current base, so the
//! app must be on the bypass list. The risk: a bypassing actor skips *every* rule of that
//! ruleset for its merges, required checks and reviews included. Only the queue's own
//! conditions stand between a bug in Schlange and the base, so give the bypass to the app only
//! (never to people), keep it in a ruleset of its own if possible, and keep the queue's
//! `merge_conditions` as strict as the protection was.

use crate::api::RepoName;
use crate::client::{Client, Error};
use crate::config::ConfigCheck;
use crate::permissions;
use crate::time::{parse_rfc3339, unix_now};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Check {
    pub name: &'static str,
    /// `OK`, `WARN`, `FAIL` or `SKIP`.
    pub status: &'static str,
    pub detail: String,
}

fn check(name: &'static str, status: &'static str, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
    }
}

/// Runs the GitHub checks. `max_delivery_age` in seconds. `config` validates each
/// repository's `.mergify.yml`; `None` reports that the schema is missing.
pub fn github_checks(
    client: &Client,
    repos: &[RepoName],
    installation: Option<u64>,
    max_delivery_age: u64,
    config: Option<&ConfigCheck>,
) -> Vec<Check> {
    github_checks_for(client, repos, installation, max_delivery_age, config, false)
}

/// [`github_checks`] for a mode. In shadow mode the app needs read access only
/// ([`permissions::SHADOW_PERMISSIONS`]; a write permission is a warning, since a shadow app
/// that can write is protected by code alone) and no ruleset exception (it never lands).
pub fn github_checks_for(
    client: &Client,
    repos: &[RepoName],
    installation: Option<u64>,
    max_delivery_age: u64,
    config: Option<&ConfigCheck>,
    shadow: bool,
) -> Vec<Check> {
    let mut out = Vec::new();
    let app = match client.app() {
        Ok(a) => {
            out.push(check(
                "github-app",
                "OK",
                format!("app `{}` (id {}) reachable", a.slug, a.id),
            ));
            a
        }
        Err(e) => {
            out.push(check("github-app", "FAIL", format!("GET /app: {e}")));
            for name in [
                "app-permissions",
                "ruleset-exception",
                "configuration",
                "webhooks",
                "rate-limit",
            ] {
                out.push(check(name, "SKIP", "the app is not reachable"));
            }
            return out;
        }
    };
    let installation = match installation {
        Some(i) => Ok(i),
        None => match repos.first() {
            Some(r) => client.repo_installation(r).map(|i| i.id),
            None => Err(Error::Auth(
                "no repository and no installation id given".into(),
            )),
        },
    };
    match installation.and_then(|id| client.installation(id)) {
        Ok(inst) => {
            let r = permissions::compare_with(&inst.permissions, permissions::for_mode(shadow));
            let (status, mut detail) = if !r.missing.is_empty() {
                ("FAIL", format!("missing: {}", r.missing.join(", ")))
            } else if !r.extra.is_empty() || !r.optional_missing.is_empty() {
                ("WARN", String::from("required permissions present"))
            } else if shadow {
                (
                    "OK",
                    String::from("read-only: exactly what shadow mode needs"),
                )
            } else {
                ("OK", String::from("exactly the permissions Schlange needs"))
            };
            if !r.extra.is_empty() {
                detail.push_str(&format!(
                    "; more than needed (W-22): {}",
                    r.extra.join(", ")
                ));
            }
            if !r.optional_missing.is_empty() {
                detail.push_str(&format!(
                    "; optional, missing: {} (pull requests changing workflows cannot be queued)",
                    r.optional_missing.join(", ")
                ));
            }
            out.push(check("app-permissions", status, detail));
            out.push(if shadow {
                check("ruleset-exception", "SKIP", "shadow mode never lands")
            } else {
                ruleset_check(client, repos, inst.id, app.id)
            });
            out.push(config_check(client, repos, inst.id, config));
            out.push(match client.rate_limit(inst.id) {
                Ok(rl) => {
                    let low = rl.limit > 0 && rl.remaining * 10 < rl.limit;
                    check(
                        "rate-limit",
                        if low { "WARN" } else { "OK" },
                        format!(
                            "{} of {} requests left, reset in {} s",
                            rl.remaining,
                            rl.limit,
                            rl.reset.saturating_sub(unix_now())
                        ),
                    )
                }
                Err(e) => check("rate-limit", "WARN", format!("GET /rate_limit: {e}")),
            });
        }
        Err(e) => {
            out.push(check(
                "app-permissions",
                "FAIL",
                format!("installation: {e}"),
            ));
            out.push(check("ruleset-exception", "SKIP", "no installation"));
            out.push(check("configuration", "SKIP", "no installation"));
            out.push(check("rate-limit", "SKIP", "no installation"));
        }
    }
    out.push(webhook_check(client, max_delivery_age));
    out
}

/// Each repository's `.mergify.yml` from its default branch, through the validator.
fn config_check(
    client: &Client,
    repos: &[RepoName],
    installation: u64,
    config: Option<&ConfigCheck>,
) -> Check {
    let Some(cfg) = config else {
        return check(
            "configuration",
            "FAIL",
            "no Mergify schema found (--schema, SCHLANGE_SCHEMA): `serve` does not start without it",
        );
    };
    let mut fails = Vec::new();
    let mut warns = Vec::new();
    let mut oks = Vec::new();
    for repo in repos {
        let api = crate::api::RepoApi {
            client: client.clone(),
            repo: repo.clone(),
            installation,
        };
        match crate::config::read_from_default_branch(&api, crate::client::Priority::Normal) {
            Ok(crate::config::Found::None) => fails.push(format!(
                "{}: no configuration file on the default branch (searched {})",
                repo.full(),
                schlange_config::config_file::NAMES.join(", ")
            )),
            Ok(crate::config::Found::Ambiguous(e)) => fails.push(format!(
                "{}: {e}; `serve` keeps the last valid configuration",
                repo.full()
            )),
            Ok(crate::config::Found::One(path, text)) => match cfg.check(&path, &text) {
                Ok(l) if l.warnings.is_empty() => {
                    oks.push(format!("{}: {path} valid", repo.full()))
                }
                Ok(l) => warns.push(format!(
                    "{}: {path} valid, {} feature(s) not supported yet: {}",
                    repo.full(),
                    l.warnings.len(),
                    l.warnings
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )),
                Err(e) => fails.push(format!(
                    "{}: {path} invalid, `serve` keeps the last valid configuration: {e}",
                    repo.full()
                )),
            },
            Err(e) => warns.push(format!("{}: configuration not read: {e}", repo.full())),
        }
    }
    let status = if !fails.is_empty() {
        "FAIL"
    } else if !warns.is_empty() {
        "WARN"
    } else {
        "OK"
    };
    let mut all = fails;
    all.extend(warns);
    all.extend(oks);
    check("configuration", status, all.join("; "))
}

fn ruleset_check(client: &Client, repos: &[RepoName], installation: u64, app_id: u64) -> Check {
    let mut problems = Vec::new();
    let mut unknown = Vec::new();
    let mut seen = 0;
    for repo in repos {
        let api = crate::api::RepoApi {
            client: client.clone(),
            repo: repo.clone(),
            installation,
        };
        let prio = crate::client::Priority::Normal;
        let list = match api.rulesets(prio) {
            Ok(l) => l,
            Err(e) => {
                unknown.push(format!("{}: {e}", repo.full()));
                continue;
            }
        };
        for rs in list {
            if rs.target.as_deref().is_some_and(|t| t != "branch") {
                continue;
            }
            let full = match api.ruleset(rs.id, prio) {
                Ok(f) => f,
                Err(e) => {
                    unknown.push(format!("{} ruleset `{}`: {e}", repo.full(), rs.name));
                    continue;
                }
            };
            if full.enforcement != "active" {
                continue;
            }
            seen += 1;
            match &full.bypass_actors {
                None => unknown.push(format!(
                    "{} ruleset `{}`: bypass list not visible to the app",
                    repo.full(),
                    full.name
                )),
                Some(actors) => {
                    let ok = actors.iter().any(|a| {
                        a.actor_type == "Integration"
                            && a.actor_id == Some(app_id)
                            && matches!(
                                a.bypass_mode.as_deref().unwrap_or("always"),
                                "always" | "exempt"
                            )
                    });
                    if !ok {
                        problems.push(format!("{} ruleset `{}`", repo.full(), full.name));
                    }
                }
            }
        }
    }
    if !problems.is_empty() {
        check(
            "ruleset-exception",
            "FAIL",
            format!(
                "the app (id {app_id}) may not bypass: {}; batch landings will be refused. {BYPASS_NOTE}",
                problems.join(", ")
            ),
        )
    } else if !unknown.is_empty() {
        check("ruleset-exception", "WARN", unknown.join("; "))
    } else {
        check(
            "ruleset-exception",
            "OK",
            format!("{seen} active branch ruleset(s), the app may bypass each. {BYPASS_NOTE}"),
        )
    }
}

/// The bypass requirement and its risk, in one line for the doctor's output.
pub const BYPASS_NOTE: &str = "Why: batches land as squash commits whose checks ran on the queue's draft, which a pull-request ruleset refuses without a bypass. Risk: a bypass skips every rule of the ruleset for the app's merges (checks, reviews); grant it to the app only and keep merge_conditions as strict as the protection.";

fn webhook_check(client: &Client, max_age: u64) -> Check {
    match client.hook_deliveries(50) {
        Ok(d) if d.is_empty() => check("webhooks", "WARN", "no webhook deliveries yet"),
        Ok(d) => {
            let newest = d
                .iter()
                .filter_map(|x| parse_rfc3339(&x.delivered_at))
                .max()
                .unwrap_or(0);
            let failed: Vec<String> = d
                .iter()
                .filter(|x| !(200..300).contains(&x.status_code))
                .map(|x| format!("{} ({})", x.event, x.status_code))
                .collect();
            let age = unix_now().saturating_sub(newest);
            if !failed.is_empty() {
                check(
                    "webhooks",
                    "WARN",
                    format!(
                        "{} of the last {} deliveries failed: {}",
                        failed.len(),
                        d.len(),
                        failed.into_iter().take(5).collect::<Vec<_>>().join(", ")
                    ),
                )
            } else if age > max_age {
                check(
                    "webhooks",
                    "WARN",
                    format!("last delivery {age} s ago (reconciliation still runs)"),
                )
            } else {
                check(
                    "webhooks",
                    "OK",
                    format!("last delivery {age} s ago, the last {} succeeded", d.len()),
                )
            }
        }
        Err(e) => check("webhooks", "WARN", format!("GET /app/hook/deliveries: {e}")),
    }
}
