//! The permissions the GitHub App needs, and no more (W-22). Mergify asks for about fifteen,
//! among them `administration: read`, `actions: write` and `members: read`. Schlange needs:
//!
//! | Permission | Level | Why |
//! |---|---|---|
//! | `metadata` | read | mandatory for every app; also rulesets (`/rules/branches`) |
//! | `contents` | write | queue branches (`POST /git/refs`), merges into them (`POST /merges`), squash landing, reading `.mergify.yml` |
//! | `pull_requests` | write | reading pull requests and files, the queue's draft pull requests, merging, labels and comments on pull requests |
//! | `checks` | write | reading check runs; the queue's own check run |
//! | `statuses` | read | commit statuses as conditions (`check-success` also matches statuses) |
//! | `workflows` | write | *optional*: only when pull requests that change `.github/workflows/` go through the queue; GitHub refuses such merge commits otherwise |
//!
//! Webhook events: `pull_request`, `check_run`, `check_suite`, `status`, `issue_comment`,
//! `push`; `branch_protection_rule` and `repository_ruleset` when GitHub offers them with these
//! permissions. Everything else is not requested; `schlange doctor` reports extra permissions
//! as a warning.

/// `(permission, level, required)`.
pub const PERMISSIONS: &[(&str, &str, bool)] = &[
    ("metadata", "read", true),
    ("contents", "write", true),
    ("pull_requests", "write", true),
    ("checks", "write", true),
    ("statuses", "read", true),
    ("workflows", "write", false),
];

pub const EVENTS: &[&str] = &[
    "pull_request",
    "check_run",
    "check_suite",
    "status",
    "issue_comment",
    "pull_request_review",
    "push",
];

fn rank(level: &str) -> u8 {
    match level {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    }
}

/// Compares an installation's permissions with the list above.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PermissionReport {
    /// Required permissions that are missing or too low: `name: have → need`.
    pub missing: Vec<String>,
    /// Optional permissions that are missing.
    pub optional_missing: Vec<String>,
    /// Granted beyond what Schlange needs.
    pub extra: Vec<String>,
}

pub fn compare(granted: &std::collections::BTreeMap<String, String>) -> PermissionReport {
    let mut r = PermissionReport::default();
    for (name, level, required) in PERMISSIONS {
        let have = granted.get(*name).map(String::as_str).unwrap_or("none");
        if rank(have) < rank(level) {
            let line = format!("{name}: {have} → {level}");
            if *required {
                r.missing.push(line);
            } else {
                r.optional_missing.push(line);
            }
        }
    }
    for (name, have) in granted {
        match PERMISSIONS.iter().find(|(n, ..)| n == name) {
            None => r.extra.push(format!("{name}: {have}")),
            Some((_, level, _)) if rank(have) > rank(level) => {
                r.extra.push(format!("{name}: {have} (needs {level})"))
            }
            Some(_) => {}
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_and_extra_permissions_are_named() {
        let granted = [
            ("metadata", "read"),
            ("contents", "read"),
            ("pull_requests", "write"),
            ("checks", "write"),
            ("statuses", "read"),
            ("administration", "read"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let r = compare(&granted);
        assert_eq!(r.missing, vec!["contents: read → write"]);
        assert_eq!(r.optional_missing, vec!["workflows: none → write"]);
        assert_eq!(r.extra, vec!["administration: read"]);
    }
}
