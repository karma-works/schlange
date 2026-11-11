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
                ruleset_check(client, repos, inst.id, app.id, config)
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

fn ruleset_check(
    client: &Client,
    repos: &[RepoName],
    installation: u64,
    app_id: u64,
    config: Option<&ConfigCheck>,
) -> Check {
    let mut problems = Vec::new();
    let mut always = Vec::new();
    let mut unknown = Vec::new();
    let mut seen = 0;
    for repo in repos {
        let api = crate::api::RepoApi {
            client: client.clone(),
            repo: repo.clone(),
            installation,
        };
        let prio = crate::client::Priority::Normal;
        let default = match api.repository(prio) {
            Ok(r) => r.default_branch,
            Err(e) => {
                unknown.push(format!(
                    "{}: cannot determine default branch: {e}",
                    repo.full()
                ));
                continue;
            }
        };
        let bases = match queue_bases(&api, config, &default) {
            Ok(bases) => bases,
            Err(e) => {
                unknown.push(format!(
                    "{}: cannot determine queue base branches: {e}",
                    repo.full()
                ));
                continue;
            }
        };
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
            if full.enforcement != "active"
                || full.target.as_deref().is_some_and(|t| t != "branch")
                || !bases
                    .iter()
                    .any(|base| ruleset_matches(&full, base, &default))
            {
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
                    let app = |mode: &str| {
                        actors.iter().any(|a| {
                            a.actor_type == "Integration"
                                && a.actor_id == Some(app_id)
                                && a.bypass_mode.as_deref().unwrap_or("always") == mode
                        })
                    };
                    if app("exempt") {
                    } else if app("always") {
                        // `always` can leave queue operations blocked; fail deployment.
                        always.push(format!("{} ruleset `{}`", repo.full(), full.name));
                    } else {
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
    } else if !always.is_empty() {
        check(
            "ruleset-exception",
            "FAIL",
            format!(
                "the app bypasses with mode `always` in {}; use `exempt`, because `always` can leave parts of the ruleset blocked. Run `bash scripts/schlange-setup.sh setup`. {BYPASS_NOTE}",
                always.join(", ")
            ),
        )
    } else if !unknown.is_empty() {
        check("ruleset-exception", "WARN", unknown.join("; "))
    } else {
        check(
            "ruleset-exception",
            "OK",
            format!(
                "{seen} active branch ruleset(s), the app may bypass with `exempt`. {BYPASS_NOTE}"
            ),
        )
    }
}

/// Queue conditions constrain the possible base branches independently of mutable PR
/// attributes. Unknown atoms stay unknown through `not` and `or`, rather than pretending a
/// made-up pull request satisfies a condition about reviews, labels or checks.
fn base_condition(
    condition: &schlange_config::conditions::Condition,
    pr: &schlange_core::model::PullRequest,
) -> Option<bool> {
    use schlange_config::conditions::Condition;
    match condition {
        Condition::Atom(atom) if atom.attribute == "base" => {
            schlange_core::eval::eval_atom(atom, pr, &Default::default()).ok()
        }
        Condition::Atom(_) => None,
        Condition::Always => Some(true),
        Condition::Not(c) => base_condition(c, pr).map(|value| !value),
        Condition::And(cs) => {
            let values: Vec<_> = cs.iter().map(|c| base_condition(c, pr)).collect();
            if values.contains(&Some(false)) {
                Some(false)
            } else if values.contains(&None) {
                None
            } else {
                Some(true)
            }
        }
        Condition::Or(cs) => {
            let values: Vec<_> = cs.iter().map(|c| base_condition(c, pr)).collect();
            if values.contains(&Some(true)) {
                Some(true)
            } else if values.contains(&None) {
                None
            } else {
                Some(false)
            }
        }
    }
}

fn queue_bases(
    api: &crate::api::RepoApi,
    config: Option<&ConfigCheck>,
    default: &str,
) -> Result<Vec<String>, Error> {
    let prio = crate::client::Priority::Normal;
    let loaded = config.and_then(|config| {
        let crate::config::Found::One(path, text) =
            crate::config::read_from_default_branch(api, prio).ok()?
        else {
            return None;
        };
        let extended = crate::config::extended_text(api, &text, prio);
        config
            .check_extended(&path, &text, Some(&api.repo.full()), |_| {
                extended.unwrap_or_else(|| Err("extended configuration unavailable".into()))
            })
            .ok()
    });
    let Some(loaded) = loaded.filter(|l| !l.rules.queue_rules.is_empty()) else {
        return Ok(vec![default.to_string()]);
    };
    let mut branches = vec![default.to_string()];
    for git_ref in api.matching_branches("", prio)? {
        if let Some(branch) = git_ref.ref_name.strip_prefix("refs/heads/") {
            branches.push(branch.to_string());
        }
    }
    branches.sort();
    branches.dedup();
    branches.retain(|base| {
        let pr = schlange_core::model::PullRequest::new(0, "", base, "", "");
        loaded.rules.queue_rules.iter().any(|queue| {
            queue
                .queue_conditions
                .iter()
                .all(|condition| base_condition(condition, &pr) != Some(false))
        })
    });
    Ok(branches)
}

fn ruleset_matches(ruleset: &crate::api::Ruleset, branch: &str, default: &str) -> bool {
    let Some(selectors) = ruleset.conditions.as_ref().and_then(|c| c.get("ref_name")) else {
        // Older API responses or an unreadable condition must not silently exempt a ruleset.
        return true;
    };
    let matches = |pattern: &serde_json::Value| {
        let pattern = pattern.as_str()?;
        match pattern {
            "~ALL" => Some(true),
            "~DEFAULT_BRANCH" => Some(branch == default),
            pattern => schlange_config::glob::glob_to_regex(pattern)
                .ok()
                .and_then(|regex| fancy_regex::Regex::new(&regex).ok())
                .and_then(|regex| regex.is_match(&format!("refs/heads/{branch}")).ok()),
        }
    };
    let included = selectors
        .get("include")
        .and_then(|v| v.as_array())
        .is_none_or(|patterns| patterns.iter().any(|p| matches(p).unwrap_or(true)));
    let excluded = selectors
        .get("exclude")
        .and_then(|v| v.as_array())
        .is_some_and(|patterns| patterns.iter().any(|p| matches(p).unwrap_or(false)));
    included && !excluded
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

#[cfg(test)]
mod tests {
    use super::*;
    use schlange_config::conditions::{Condition, parse_atom};

    #[test]
    fn base_scope_preserves_unknown_atoms_under_boolean_combinators() {
        let pr = schlange_core::model::PullRequest::new(1, "author", "main", "feature", "head");
        let base = Condition::Atom(parse_atom("base = main").unwrap());
        let other = Condition::Atom(parse_atom("base = release").unwrap());
        let mutable = Condition::Atom(parse_atom("label = ready").unwrap());
        assert_eq!(base_condition(&base, &pr), Some(true));
        assert_eq!(base_condition(&other, &pr), Some(false));
        assert_eq!(
            base_condition(&Condition::Not(Box::new(mutable.clone())), &pr),
            None
        );
        assert_eq!(
            base_condition(&Condition::And(vec![other.clone(), mutable.clone()]), &pr),
            Some(false)
        );
        assert_eq!(
            base_condition(&Condition::Or(vec![other, mutable]), &pr),
            None
        );
        assert_eq!(
            base_condition(&Condition::Not(Box::new(base)), &pr),
            Some(false)
        );
    }

    #[test]
    fn ruleset_excludes_override_recursive_ref_globs() {
        let ruleset: crate::api::Ruleset = serde_json::from_value(serde_json::json!({
            "id": 1, "name": "release", "conditions": {"ref_name": {
                "include": ["refs/heads/release/**/*"], "exclude": ["refs/heads/release/private/**"]
            }}
        }))
        .unwrap();
        assert!(ruleset_matches(&ruleset, "release/public/1", "main"));
        assert!(!ruleset_matches(&ruleset, "release/private/1", "main"));
        assert!(!ruleset_matches(&ruleset, "main", "main"));
    }
}
