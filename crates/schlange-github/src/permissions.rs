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
//!
//! **Shadow mode** ([`SHADOW_PERMISSIONS`]) needs only read access: `metadata`, `contents`,
//! `pull_requests`, `checks` and `statuses`, all `read`. A shadow app with nothing more cannot
//! write to GitHub whatever the code does; `schlange doctor --shadow` warns about any write
//! permission, and `serve --shadow` asks for read-only installation tokens.

/// `(permission, level, required)`.
pub const PERMISSIONS: &[(&str, &str, bool)] = &[
    ("metadata", "read", true),
    ("contents", "write", true),
    ("pull_requests", "write", true),
    ("checks", "write", true),
    ("statuses", "read", true),
    ("workflows", "write", false),
];

/// The permissions of a shadow-mode app (S-SHADOW): read access to what Schlange reads.
pub const SHADOW_PERMISSIONS: &[(&str, &str, bool)] = &[
    ("metadata", "read", true),
    ("contents", "read", true),
    ("pull_requests", "read", true),
    ("checks", "read", true),
    ("statuses", "read", true),
];

/// The permission list of a mode: shadow or acting.
pub fn for_mode(shadow: bool) -> &'static [(&'static str, &'static str, bool)] {
    if shadow {
        SHADOW_PERMISSIONS
    } else {
        PERMISSIONS
    }
}

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
    compare_with(granted, PERMISSIONS)
}

/// Compares an installation's permissions with a list ([`PERMISSIONS`], [`SHADOW_PERMISSIONS`]).
pub fn compare_with(
    granted: &std::collections::BTreeMap<String, String>,
    list: &[(&str, &str, bool)],
) -> PermissionReport {
    let mut r = PermissionReport::default();
    for (name, level, required) in list {
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
        match list.iter().find(|(n, ..)| n == name) {
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

    #[test]
    fn a_read_only_app_has_what_shadow_mode_needs_and_write_access_is_extra() {
        let read_only = SHADOW_PERMISSIONS
            .iter()
            .map(|(a, b, _)| (a.to_string(), b.to_string()))
            .collect();
        let r = compare_with(&read_only, SHADOW_PERMISSIONS);
        assert_eq!(r, PermissionReport::default());
        // The same app cannot act: every write permission is missing.
        assert_eq!(
            compare(&read_only).missing,
            vec![
                "contents: read → write",
                "pull_requests: read → write",
                "checks: read → write"
            ]
        );
        // An acting app used for shadow mode works, but could write: reported as extra.
        let acting = PERMISSIONS
            .iter()
            .map(|(a, b, _)| (a.to_string(), b.to_string()))
            .collect();
        let r = compare_with(&acting, SHADOW_PERMISSIONS);
        assert!(r.missing.is_empty());
        assert_eq!(
            r.extra,
            vec![
                "checks: write (needs read)",
                "contents: write (needs read)",
                "pull_requests: write (needs read)",
                "workflows: write"
            ]
        );
    }
}
