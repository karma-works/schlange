//! The permissions the GitHub App needs, and no more (W-22). Mergify asks for about fifteen,
//! among them `administration: read`, `actions: write` and `members: read`. Schlange needs:
//!
//! | Permission | Level | Why |
//! |---|---|---|
//! | `metadata` | read | mandatory for every app; also rulesets (`/rules/branches`) |
//! | `contents` | write | queue branches (`POST /git/refs`), merges into them (`POST /merges`), squash landing, reading `.mergify.yml` |
//! | `pull_requests` | write | reading pull requests and files, the queue's draft pull requests, merging, labels and comments on pull requests |
//! | `issues` | read | GitHub sends `issue_comment` webhooks (the comment commands) only to apps with it |
//! | `checks` | write | reading check runs; the queue's own check run |
//! | `actions` | read | resolve workflow identity for GitHub Actions checks |
//! | `actions` | write | *optional*: only for the `github_actions` action (workflow dispatch); GitHub refuses the dispatch otherwise |
//! | `statuses` | read | commit statuses as conditions (`check-success` also matches statuses) |
//! | `workflows` | write | *optional*: only when pull requests that change `.github/workflows/` go through the queue; GitHub refuses such merge commits otherwise |
//!
//! Webhook events: `pull_request`, `check_run`, `check_suite`, `status`, `issue_comment`,
//! `pull_request_review`, `push`; `branch_protection_rule` and `repository_ruleset` when GitHub offers them with these
//! permissions. Everything else is not requested; `schlange doctor` reports extra permissions
//! as a warning.
//!
//! **Shadow mode** ([`SHADOW_PERMISSIONS`]) needs only read access: `metadata`, `contents`,
//! `issues`, `pull_requests`, `checks`, `actions` and `statuses`, all `read`. A shadow app with nothing more cannot
//! write to GitHub whatever the code does; `schlange doctor --shadow` warns about any write
//! permission, and `serve --shadow` asks for read-only installation tokens.

/// `(permission, level, required)`. A permission may appear twice: required at one level,
/// optional at a higher one (`actions`).
pub const PERMISSIONS: &[(&str, &str, bool)] = &[
    ("metadata", "read", true),
    ("contents", "write", true),
    ("pull_requests", "write", true),
    ("issues", "read", true),
    ("checks", "write", true),
    ("actions", "read", true),
    ("actions", "write", false),
    ("statuses", "read", true),
    ("workflows", "write", false),
];

/// What does not work without an optional permission (`schlange doctor`).
pub fn optional_reason(name: &str) -> &'static str {
    match name {
        "actions" => "the `github_actions` action cannot dispatch workflows",
        "workflows" => "pull requests changing workflows cannot be queued",
        _ => "an optional feature is unavailable",
    }
}

/// The highest level of `name` in `list`, or `None` if it is not listed.
pub fn highest_level(list: &[(&str, &str, bool)], name: &str) -> Option<&'static str> {
    list.iter()
        .filter(|(n, ..)| *n == name)
        .map(|(_, l, _)| *l)
        .max_by_key(|l| rank(l))
        .map(level_str)
}

fn level_str(l: &str) -> &'static str {
    match l {
        "admin" => "admin",
        "write" => "write",
        _ => "read",
    }
}

/// The permissions of a shadow-mode app (S-SHADOW): read access to what Schlange reads.
pub const SHADOW_PERMISSIONS: &[(&str, &str, bool)] = &[
    ("metadata", "read", true),
    ("contents", "read", true),
    ("pull_requests", "read", true),
    ("issues", "read", true),
    ("checks", "read", true),
    ("actions", "read", true),
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

pub fn rank(level: &str) -> u8 {
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
            } else if !r
                .missing
                .iter()
                .any(|m| m.starts_with(&format!("{name}: ")))
            {
                r.optional_missing.push(line);
            }
        }
    }
    for (name, have) in granted {
        match highest_level(list, name) {
            None => r.extra.push(format!("{name}: {have}")),
            Some(level) if rank(have) > rank(level) => {
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

    fn map(entries: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        entries
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    fn required_with(actions: &str) -> std::collections::BTreeMap<String, String> {
        map(&[
            ("metadata", "read"),
            ("contents", "write"),
            ("pull_requests", "write"),
            ("issues", "read"),
            ("checks", "write"),
            ("actions", actions),
            ("statuses", "read"),
        ])
    }

    #[test]
    fn actions_read_is_required_and_actions_write_optional() {
        let r = compare(&required_with("read"));
        assert!(r.missing.is_empty(), "{r:?}");
        assert!(r.extra.is_empty(), "{r:?}");
        assert!(
            r.optional_missing
                .iter()
                .any(|l| l == "actions: read → write"),
            "{r:?}"
        );
        let r = compare(&required_with("write"));
        assert!(r.missing.is_empty(), "{r:?}");
        assert!(r.extra.is_empty(), "actions: write is not extra: {r:?}");
        assert!(
            !r.optional_missing.iter().any(|l| l.starts_with("actions")),
            "{r:?}"
        );
        let r = compare(&required_with("none"));
        assert!(
            r.missing.iter().any(|l| l == "actions: none → read"),
            "{r:?}"
        );
        assert!(optional_reason("actions").contains("dispatch"));
        assert!(optional_reason("workflows").contains("workflows"));
    }

    #[test]
    fn missing_and_extra_permissions_are_named() {
        let granted = [
            ("metadata", "read"),
            ("contents", "read"),
            ("pull_requests", "write"),
            ("checks", "write"),
            ("actions", "read"),
            ("statuses", "read"),
            ("administration", "read"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let r = compare(&granted);
        assert_eq!(
            r.missing,
            vec!["contents: read → write", "issues: none → read"]
        );
        assert_eq!(
            r.optional_missing,
            vec!["actions: read → write", "workflows: none → write"]
        );
        assert_eq!(r.extra, vec!["administration: read"]);
    }

    #[test]
    fn check_identity_requires_actions_read_in_both_modes() {
        for permissions in [PERMISSIONS, SHADOW_PERMISSIONS] {
            let granted = permissions
                .iter()
                .filter(|(name, _, _)| *name != "actions")
                .map(|(name, level, _)| (name.to_string(), level.to_string()))
                .collect();
            assert_eq!(
                compare_with(&granted, permissions).missing,
                vec!["actions: none → read"]
            );
        }
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
                "actions: write (needs read)",
                "checks: write (needs read)",
                "contents: write (needs read)",
                "pull_requests: write (needs read)",
                "workflows: write"
            ]
        );
    }
}
