//! Golden files for squash commit messages (MQ-20): one file per `commit_message_format`
//! combination of title × body × trailers, for a fixed pull request. A part the queue leaves to
//! the forge (`inherit`) is shown as `<inherited …>`; how each forge fills it is tested with
//! that forge (simulated forge scenario `p1-commit-message.yaml`, fake GitHub e2e).
//! `UPDATE_GOLDEN=1` rewrites the files.

use std::path::Path;

use schlange_config::rules::{BodyFormat, CommitMessageFormat, TitleFormat, Trailer};
use schlange_core::message::compose;
use schlange_core::model::PullRequest;

fn pr() -> PullRequest {
    let mut p = PullRequest::new(42, "alice", "main", "fix-login", "abc");
    p.title = "Fix the login redirect".into();
    p.body = "The redirect lost the query string.\n\nDepends-On: #41\n".into();
    p.commit_authors = vec![
        "alice <alice@example.com>".into(),
        "Bob Builder <bob@example.com>".into(),
        "Bob Builder <bob@example.com>".into(),
        "Carol <7+carol@users.noreply.github.com>".into(),
    ];
    p.approved_by = vec!["dave".into(), "erin".into()];
    p
}

fn render(f: &CommitMessageFormat) -> String {
    let m = compose(Some(f), &pr(), "frank");
    let title = m
        .title
        .clone()
        .unwrap_or_else(|| "<inherited title>".into());
    let body = m.full_body("<inherited body>");
    format!("{title}\n\n{body}\n")
}

#[test]
fn every_combination() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/commit-message");
    std::fs::create_dir_all(&dir).unwrap();
    let all = [
        Trailer::CoAuthoredBy,
        Trailer::ApprovedBy,
        Trailer::MergedBy,
    ];
    let mut count = 0;
    for (title, tname) in [
        (TitleFormat::Inherit, "inherit"),
        (TitleFormat::PrTitle, "pr-title"),
    ] {
        for (body, bname) in [
            (BodyFormat::Inherit, "inherit"),
            (BodyFormat::PrBody, "pr-body"),
            (BodyFormat::Empty, "empty"),
        ] {
            for mask in 0..8u8 {
                let trailers: Vec<Trailer> = all
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, t)| *t)
                    .collect();
                let tr = if trailers.is_empty() {
                    "none".to_string()
                } else {
                    trailers
                        .iter()
                        .map(|t| match t {
                            Trailer::CoAuthoredBy => "co-authored-by",
                            Trailer::ApprovedBy => "approved-by",
                            Trailer::MergedBy => "merged-by",
                        })
                        .collect::<Vec<_>>()
                        .join("+")
                };
                let f = CommitMessageFormat {
                    title,
                    body,
                    trailers,
                };
                let actual = render(&f);
                let path = dir.join(format!("title-{tname}.body-{bname}.trailers-{tr}.txt"));
                if std::env::var_os("UPDATE_GOLDEN").is_some() {
                    std::fs::write(&path, &actual).unwrap();
                } else {
                    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
                        panic!("{} missing; run with UPDATE_GOLDEN=1", path.display())
                    });
                    assert_eq!(expected, actual, "{}", path.display());
                }
                count += 1;
            }
        }
    }
    assert_eq!(count, 48);
}

#[test]
fn the_default_is_the_forges_own_message() {
    assert!(compose(None, &pr(), "frank").is_inherited());
}
