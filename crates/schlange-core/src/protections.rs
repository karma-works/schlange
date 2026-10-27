//! Built-in merge protections (docs: merge-protections/builtin) and the other reasons that keep
//! an open pull request out of the queue. Every reason is a [`Hold`] in the engine state
//! (`State::holds`), so `pr explain` can name it.
//!
//! - `Depends-On`: lines `Depends-On: #123`, `Depends-On: owner/repo#123` or a pull request URL
//!   in the body. Every named pull request must be merged. An open one keeps the pull request
//!   waiting; one closed without merge, one that does not exist or cannot be read, and one of
//!   another owner can never be satisfied and are reported as failures (the pull request
//!   stays out of the queue until the body changes). More than [`MAX_DEPENDENCIES`] lines
//!   resolve none of them. A circular chain never resolves; it is named in the reason.
//! - `Merge-After`: a timestamp in the body (see [`crate::time`]); the pull request waits until
//!   then. A value that is not a timestamp holds it until the line is fixed.
//! - Configuration changed: a pull request against the default branch that changes the
//!   configuration file waits while the new configuration does not validate.
//!
//! Our choices: the key words are matched without regard to letter case, with spaces around
//! the colon allowed; with several `Merge-After` lines the latest time counts.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::engine::State;
use crate::model::{DependencyState, PullRequest, Time};
use crate::time::{TimeError, format_utc, parse_timestamp};

/// At most this many dependencies are resolved (docs).
pub const MAX_DEPENDENCIES: usize = 50;

/// Why an open pull request is not (or no longer) in the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    /// `depends-on`, `merge-after`, `configuration-changed`, `queue-command`,
    /// `manually-dequeued`.
    pub kind: String,
    pub text: String,
    /// It can never pass as the pull request is now (a failure, not a wait).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

impl Hold {
    pub fn wait(kind: &str, text: impl Into<String>) -> Self {
        Hold {
            kind: kind.into(),
            text: text.into(),
            failed: false,
        }
    }

    pub fn fail(kind: &str, text: impl Into<String>) -> Self {
        Hold {
            kind: kind.into(),
            text: text.into(),
            failed: true,
        }
    }
}

/// A `Depends-On` reference.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reference {
    /// `owner/repo`, `None` for the repository itself (`#123`).
    pub repo: Option<String>,
    pub number: u64,
}

impl Reference {
    /// `owner/repo#n`, with `repository` for a local reference.
    pub fn full(&self, repository: &str) -> String {
        match &self.repo {
            Some(r) => format!("{r}#{}", self.number),
            None if repository.is_empty() => format!("#{}", self.number),
            None => format!("{repository}#{}", self.number),
        }
    }

    /// Whether it names a pull request of `repository`.
    pub fn is_local(&self, repository: &str) -> bool {
        match &self.repo {
            None => true,
            Some(r) => r.eq_ignore_ascii_case(repository),
        }
    }
}

/// The value after `Key:` on lines of the body, for a key matched without letter case.
fn header_values<'a>(body: &'a str, key: &str) -> Vec<&'a str> {
    body.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(key).then(|| v.trim())
        })
        .collect()
}

/// Parses one `Depends-On` value; `Err` with the value when it is no reference.
pub fn parse_reference(value: &str) -> Result<Reference, String> {
    let v = value.trim();
    let number = |s: &str| s.parse::<u64>().ok().filter(|n| *n > 0);
    if let Some(n) = v.strip_prefix('#') {
        return number(n)
            .map(|number| Reference { repo: None, number })
            .ok_or_else(|| v.to_string());
    }
    for scheme in ["https://", "http://"] {
        if let Some(rest) = v.strip_prefix(scheme) {
            // host/owner/repo/pull/n
            let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
            if let [_host, owner, repo, "pull" | "pulls", n, ..] = parts[..]
                && let Some(number) = number(n)
            {
                return Ok(Reference {
                    repo: Some(format!("{owner}/{repo}")),
                    number,
                });
            }
            return Err(v.to_string());
        }
    }
    if let Some((repo, n)) = v.rsplit_once('#')
        && let Some((owner, name)) = repo.split_once('/')
        && !owner.is_empty()
        && !name.is_empty()
        && !name.contains('/')
        && let Some(number) = number(n)
    {
        return Ok(Reference {
            repo: Some(repo.to_string()),
            number,
        });
    }
    Err(v.to_string())
}

/// The `Depends-On` references of a body (each once) and the values that are no reference.
pub fn depends_on(body: &str) -> (Vec<Reference>, Vec<String>) {
    let mut refs = Vec::new();
    let mut bad = Vec::new();
    for v in header_values(body, "Depends-On") {
        match parse_reference(v) {
            Ok(r) if !refs.contains(&r) => refs.push(r),
            Ok(_) => {}
            Err(b) => bad.push(b),
        }
    }
    (refs, bad)
}

/// The `depends-on` attribute: every reference as `owner/repo#n`.
pub fn depends_on_attribute(pr: &PullRequest, repository: &str) -> Vec<String> {
    depends_on(&pr.body)
        .0
        .iter()
        .map(|r| r.full(repository))
        .collect()
}

/// The `Merge-After` time of a body (Unix seconds): `Ok(None)` without a line.
pub fn merge_after(body: &str) -> Result<Option<i64>, (String, TimeError)> {
    let mut latest: Option<i64> = None;
    for v in header_values(body, "Merge-After") {
        let t = parse_timestamp(v).map_err(|e| (v.to_string(), e))?;
        latest = Some(latest.map_or(t, |l| l.max(t)));
    }
    Ok(latest)
}

/// The state of a dependency: the engine's own view of a pull request it knows wins, except
/// that `merged` from either side is final.
fn dependency_state(
    state: &State,
    pr: &PullRequest,
    r: &Reference,
    repository: &str,
) -> Option<DependencyState> {
    let full = r.full(repository);
    let read = pr
        .dependencies
        .iter()
        .find(|d| d.reference == full || (r.repo.is_none() && d.reference == r.full("")))
        .map(|d| d.state);
    let own = r
        .is_local(repository)
        .then(|| state.pull_requests.get(&r.number))
        .flatten()
        .map(|p| {
            if p.merged {
                DependencyState::Merged
            } else if p.closed {
                DependencyState::Closed
            } else {
                DependencyState::Open
            }
        });
    match (own, read) {
        (_, Some(DependencyState::Merged)) | (Some(DependencyState::Merged), _) => {
            Some(DependencyState::Merged)
        }
        (Some(o), _) => Some(o),
        (None, r) => r,
    }
}

/// A circular chain through local `Depends-On` lines back to `start`, as known to the engine.
fn cycle(state: &State, start: u64, repository: &str) -> Option<Vec<u64>> {
    fn walk(
        state: &State,
        at: u64,
        start: u64,
        repository: &str,
        path: &mut Vec<u64>,
        seen: &mut BTreeSet<u64>,
    ) -> bool {
        let Some(pr) = state.pull_requests.get(&at) else {
            return false;
        };
        if pr.merged {
            return false;
        }
        for r in depends_on(&pr.body).0 {
            if !r.is_local(repository) {
                continue;
            }
            if r.number == start {
                path.push(start);
                return true;
            }
            if seen.insert(r.number) {
                path.push(r.number);
                if walk(state, r.number, start, repository, path, seen) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }
    let mut path = vec![start];
    let mut seen = BTreeSet::from([start]);
    walk(state, start, start, repository, &mut path, &mut seen).then_some(path)
}

/// The built-in protections that hold `pr` now, and the time to look again (`Merge-After`).
pub fn protections(state: &State, pr: &PullRequest, now: Time) -> (Vec<Hold>, Option<Time>) {
    let repository = state.host.repository.clone();
    let mut holds = Vec::new();
    let mut wake = None;
    // Depends-On.
    let (refs, bad) = depends_on(&pr.body);
    if refs.len() + bad.len() > MAX_DEPENDENCIES {
        holds.push(Hold::fail(
            "depends-on",
            format!(
                "the description names {} dependencies; at most {MAX_DEPENDENCIES} are resolved, \
                 so none is",
                refs.len() + bad.len()
            ),
        ));
    } else {
        for b in &bad {
            holds.push(Hold::fail(
                "depends-on",
                format!("`Depends-On: {b}` is not a pull request reference"),
            ));
        }
        let owner = repository.split('/').next().unwrap_or("");
        let mut open = Vec::new();
        for r in &refs {
            let full = r.full(&repository);
            if let Some(repo) = &r.repo
                && !owner.is_empty()
                && !repo
                    .split('/')
                    .next()
                    .is_some_and(|o| o.eq_ignore_ascii_case(owner))
            {
                holds.push(Hold::fail(
                    "depends-on",
                    format!("{full} belongs to another owner than this repository"),
                ));
                continue;
            }
            match dependency_state(state, pr, r, &repository) {
                Some(DependencyState::Merged) => {}
                Some(DependencyState::Open) => open.push(full),
                Some(DependencyState::Closed) => holds.push(Hold::fail(
                    "depends-on",
                    format!("{full} was closed without being merged"),
                )),
                Some(DependencyState::OtherOwner) => holds.push(Hold::fail(
                    "depends-on",
                    format!("{full} belongs to another owner than this repository"),
                )),
                Some(DependencyState::Missing) | None => holds.push(Hold::fail(
                    "depends-on",
                    format!("{full} cannot be resolved (it does not exist or cannot be read)"),
                )),
            }
        }
        if !open.is_empty() {
            let mut text = format!("waits for {} to be merged", open.join(", "));
            if let Some(c) = cycle(state, pr.number, &repository) {
                text.push_str(&format!(
                    "; circular dependency {} never resolves",
                    c.iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(" → ")
                ));
            }
            holds.push(Hold::wait("depends-on", text));
        }
    }
    // Merge-After.
    let unix_now = state.host.epoch as i64 + now as i64;
    match merge_after(&pr.body) {
        Ok(None) => {}
        Ok(Some(t)) if t > unix_now => {
            holds.push(Hold::wait(
                "merge-after",
                format!("Merge-After: waits until {}", format_utc(t)),
            ));
            let at = (t - state.host.epoch as i64).max(0) as Time;
            wake = Some(at);
        }
        Ok(Some(_)) => {}
        Err((value, TimeError::UnsupportedZone(zone))) => holds.push(Hold::fail(
            "merge-after",
            format!(
                "Merge-After `{value}`: the time zone `{zone}` is not supported by Schlange; \
                 write the time in UTC or with an offset"
            ),
        )),
        Err((value, TimeError::Invalid)) => holds.push(Hold::fail(
            "merge-after",
            format!("Merge-After `{value}` is not a valid timestamp"),
        )),
    }
    // Configuration changed.
    if let Some(c) = &pr.config_change
        && let Some(e) = &c.error
    {
        holds.push(Hold::fail(
            "configuration-changed",
            format!("the changed configuration does not validate: {e}"),
        ));
    }
    (holds, wake)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Dependency;

    fn state_with(prs: &[(u64, &str, bool, bool)]) -> State {
        let mut s = State::default();
        s.host.repository = "acme/app".into();
        for (n, body, merged, closed) in prs {
            let mut p = PullRequest::new(*n, "a", "main", &format!("b{n}"), &format!("s{n}"));
            p.body = body.to_string();
            p.merged = *merged;
            p.closed = *closed || *merged;
            s.pull_requests.insert(*n, p);
        }
        s
    }

    #[test]
    fn references() {
        assert_eq!(
            parse_reference("#12"),
            Ok(Reference {
                repo: None,
                number: 12
            })
        );
        assert_eq!(
            parse_reference("org/repo#123").unwrap().full("x/y"),
            "org/repo#123"
        );
        assert_eq!(
            parse_reference("https://github.com/org/repo/pull/123")
                .unwrap()
                .full(""),
            "org/repo#123"
        );
        for bad in [
            "123",
            "#",
            "#x",
            "org#1",
            "https://github.com/org/repo/issues/1",
            "a/b/c#1",
        ] {
            assert!(parse_reference(bad).is_err(), "{bad}");
        }
        let (refs, bad) =
            depends_on("Fixes a bug.\n\ndepends-on: #3\nDepends-On : #3\nDepends-On: nope\n");
        assert_eq!(refs.len(), 1, "duplicates count once");
        assert_eq!(bad, vec!["nope".to_string()]);
    }

    #[test]
    fn hold_release_failures_and_cycles() {
        let s = state_with(&[
            (1, "Depends-On: #2", false, false),
            (2, "", false, false),
            (3, "Depends-On: #4", false, false),
            (4, "", false, true),
            (5, "Depends-On: #6", false, false),
            (6, "Depends-On: #5", false, false),
            (7, "Depends-On: #7", false, false),
            (8, "Depends-On: other/app#1", false, false),
            (9, "Depends-On: #99", false, false),
        ]);
        let holds = |n: u64| protections(&s, &s.pull_requests[&n], 0).0;
        let h = holds(1);
        assert_eq!(h.len(), 1);
        assert!(!h[0].failed && h[0].text.contains("acme/app#2"), "{h:?}");
        assert!(holds(3)[0].failed, "closed without merge");
        assert!(holds(5)[0].text.contains("#5 → #6 → #5"), "{:?}", holds(5));
        assert!(holds(7)[0].text.contains("circular"), "self-reference");
        assert!(holds(8)[0].failed, "another owner");
        assert!(holds(9)[0].failed, "cannot be resolved");
        let mut s2 = s.clone();
        s2.pull_requests.get_mut(&2).unwrap().merged = true;
        assert!(protections(&s2, &s2.pull_requests[&1], 0).0.is_empty());
        // The forge's resolution of a pull request the engine does not know.
        let mut p = s.pull_requests[&9].clone();
        p.dependencies = vec![Dependency {
            reference: "acme/app#99".into(),
            state: DependencyState::Merged,
        }];
        assert!(protections(&s, &p, 0).0.is_empty());
        // The limit.
        let body: String = (1..=51)
            .map(|n| format!("Depends-On: #{}\n", n + 100))
            .collect();
        let mut p = s.pull_requests[&2].clone();
        p.body = body;
        let h = protections(&s, &p, 0).0;
        assert_eq!(h.len(), 1);
        assert!(h[0].failed && h[0].text.contains("51"), "{h:?}");
    }

    #[test]
    fn merge_after_waits_and_invalid_blocks() {
        let mut s = state_with(&[(1, "Merge-After: 1970-01-01T01:00:00Z", false, false)]);
        s.host.epoch = 0;
        let (h, wake) = protections(&s, &s.pull_requests[&1], 10);
        assert_eq!(h[0].kind, "merge-after");
        assert_eq!(wake, Some(3600));
        assert!(protections(&s, &s.pull_requests[&1], 3600).0.is_empty());
        s.pull_requests.get_mut(&1).unwrap().body = "Merge-After: next tuesday".into();
        let (h, wake) = protections(&s, &s.pull_requests[&1], 10);
        assert!(h[0].failed && wake.is_none());
        s.pull_requests.get_mut(&1).unwrap().body =
            "Merge-After: 2030-01-01T00:00[Europe/Paris]".into();
        assert!(protections(&s, &s.pull_requests[&1], 10).0[0].failed);
    }
}
