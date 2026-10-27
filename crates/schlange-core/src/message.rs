//! The squash commit message of a landing (`commit_message_format`, docs:
//! merge-queue/merge-strategies and workflow/actions/merge).
//!
//! The engine decides what it can: a title from the pull request (`pr-title`), a body from
//! the pull request (`pr-body`) or none (`empty`), and the trailers. `inherit` (the default)
//! leaves that part to the forge: on GitHub the repository's squash merge settings, in the
//! simulated forge the definition in `schlange_forge::sim` (title `<title> (#<n>)`, body one
//! `* <message>` line per commit when there is more than one commit). Trailers are appended
//! to the body after a blank line; with an inherited body the adapter builds the inherited
//! body itself and appends them.
//!
//! Our choices where the documentation is silent:
//! - `pr-title` is the title followed by ` (#<number>)`, as GitHub writes squash titles;
//! - `Co-authored-by: Name <email>` for each distinct commit author other than the pull
//!   request's author, in commit order;
//! - `Approved-by: <login>` for each reviewer whose latest review approves;
//! - `Merged-by: <login>`: whoever queued the pull request with a command or checkbox, else
//!   the queue's own app.

use serde::{Deserialize, Serialize};

use schlange_config::rules::{BodyFormat, CommitMessageFormat, TitleFormat, Trailer};

use crate::model::PullRequest;

/// The message of one squash commit. `None` parts are inherited from the forge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitMessage {
    pub number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Trailer lines (`Key: value`), appended to the body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trailers: Vec<String>,
}

impl CommitMessage {
    /// Nothing decided: the forge's own squash message.
    pub fn is_inherited(&self) -> bool {
        self.title.is_none() && self.body.is_none() && self.trailers.is_empty()
    }

    /// The full body: `body` (or the inherited body the adapter computed), then the trailers.
    pub fn full_body(&self, inherited_body: &str) -> String {
        let body = self.body.as_deref().unwrap_or(inherited_body).trim_end();
        if self.trailers.is_empty() {
            return body.to_string();
        }
        let trailers = self.trailers.join("\n");
        if body.is_empty() {
            trailers
        } else {
            format!("{body}\n\n{trailers}")
        }
    }
}

/// The login part of `Name <email>`-less authors, or the whole entry.
fn author_matches(entry: &str, login: &str) -> bool {
    let name = entry.split('<').next().unwrap_or("").trim();
    let email = entry
        .split('<')
        .nth(1)
        .and_then(|e| e.strip_suffix('>'))
        .unwrap_or("");
    name.eq_ignore_ascii_case(login)
        || email.split('@').next().is_some_and(|local| {
            local.eq_ignore_ascii_case(login)
                || local
                    .split_once('+')
                    .is_some_and(|(_, l)| l.eq_ignore_ascii_case(login))
        })
}

/// Composes the message of `pr` under `format`; `merged_by` is who queued it (or the app).
pub fn compose(
    format: Option<&CommitMessageFormat>,
    pr: &PullRequest,
    merged_by: &str,
) -> CommitMessage {
    let mut m = CommitMessage {
        number: pr.number,
        ..Default::default()
    };
    let Some(f) = format else {
        return m;
    };
    if f.title == TitleFormat::PrTitle {
        m.title = Some(format!("{} (#{})", pr.title, pr.number));
    }
    m.body = match f.body {
        BodyFormat::Inherit => None,
        BodyFormat::PrBody => Some(pr.body.trim_end().to_string()),
        BodyFormat::Empty => Some(String::new()),
    };
    for t in &f.trailers {
        match t {
            Trailer::CoAuthoredBy => {
                let mut seen: Vec<&str> = Vec::new();
                for a in &pr.commit_authors {
                    if author_matches(a, &pr.author) || seen.contains(&a.as_str()) {
                        continue;
                    }
                    seen.push(a);
                    m.trailers.push(format!("Co-authored-by: {a}"));
                }
            }
            Trailer::ApprovedBy => {
                for r in &pr.approved_by {
                    m.trailers.push(format!("Approved-by: {r}"));
                }
            }
            Trailer::MergedBy => m.trailers.push(format!("Merged-by: {merged_by}")),
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherit_by_default() {
        let pr = PullRequest::new(3, "a", "main", "b", "s");
        assert!(compose(None, &pr, "bot").is_inherited());
        assert!(compose(Some(&CommitMessageFormat::default()), &pr, "bot").is_inherited());
    }

    #[test]
    fn authors_are_matched_by_name_or_noreply_email() {
        assert!(author_matches("alice <alice@example.com>", "alice"));
        assert!(author_matches(
            "Alice A <123+alice@users.noreply.github.com>",
            "alice"
        ));
        assert!(!author_matches("bob <bob@example.com>", "alice"));
    }

    #[test]
    fn full_body_appends_trailers() {
        let m = CommitMessage {
            number: 1,
            title: None,
            body: None,
            trailers: vec!["Merged-by: x".into()],
        };
        assert_eq!(m.full_body("* one\n"), "* one\n\nMerged-by: x");
        assert_eq!(m.full_body(""), "Merged-by: x");
    }
}
