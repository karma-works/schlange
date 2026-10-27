//! Comment commands (docs: commands): recognition and argument parsing, and the checkboxes in
//! the queue's own comments.
//!
//! Recognition, as documented:
//! - the command starts the comment (leading blank lines and spaces are skipped; a command
//!   after a line of text is not one);
//! - the prefix is `@mergifyio` or `@mergify`, in any letter case, followed by exactly one
//!   space and the command name; `@schlange` is accepted too, since Schlange answers as
//!   itself;
//! - arguments are on the same line; the lines below are left alone.
//!
//! Our choices where the documentation says nothing: command names are matched without
//! regard to ASCII letter case; arguments are split at spaces, and a double- or
//! single-quoted argument may contain spaces; a prefix without a command, or an unknown
//! command name, is answered (so a typo does not go unnoticed).

/// The commands Schlange knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Name {
    Queue,
    /// Deprecated alias of `queue`.
    Requeue,
    Dequeue,
    Refresh,
    Backport,
    Copy,
    Rebase,
    Squash,
    Update,
}

impl Name {
    pub fn as_str(self) -> &'static str {
        match self {
            Name::Queue => "queue",
            Name::Requeue => "requeue",
            Name::Dequeue => "dequeue",
            Name::Refresh => "refresh",
            Name::Backport => "backport",
            Name::Copy => "copy",
            Name::Rebase => "rebase",
            Name::Squash => "squash",
            Name::Update => "update",
        }
    }

    fn from_word(w: &str) -> Option<Name> {
        Some(match w.to_ascii_lowercase().as_str() {
            "queue" => Name::Queue,
            "requeue" => Name::Requeue,
            "dequeue" => Name::Dequeue,
            "refresh" => Name::Refresh,
            "backport" => Name::Backport,
            "copy" => Name::Copy,
            "rebase" => Name::Rebase,
            "squash" => Name::Squash,
            "update" => Name::Update,
            _ => return None,
        })
    }

    /// Commands whose action Schlange does not have yet.
    pub fn not_built(self) -> bool {
        matches!(
            self,
            Name::Backport | Name::Copy | Name::Rebase | Name::Squash | Name::Update
        )
    }
}

/// What a comment says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Not addressed to the queue.
    NotACommand,
    Command {
        name: Name,
        args: Vec<String>,
    },
    /// The prefix, then a word that is no command.
    Unknown {
        word: String,
    },
    /// A known command with arguments it does not take, or an unterminated quote.
    Malformed {
        name: Name,
        message: String,
    },
}

const PREFIXES: [&str; 3] = ["@mergifyio", "@mergify", "@schlange"];

/// Splits the argument text at spaces; quotes group.
fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed".into());
    }
    if started {
        args.push(cur);
    }
    Ok(args)
}

/// Parses a comment body.
pub fn parse(body: &str) -> Parsed {
    let text = body.trim_start();
    let line = text.lines().next().unwrap_or("").trim_end();
    let lower = line.to_ascii_lowercase();
    let Some(prefix) = PREFIXES.iter().find(|p| lower.starts_with(*p)) else {
        return Parsed::NotACommand;
    };
    let rest = &line[prefix.len()..];
    if rest.is_empty() {
        return Parsed::Unknown {
            word: String::new(),
        };
    }
    // Exactly one space after the prefix (`@mergifyiox` is someone else).
    let Some(rest) = rest.strip_prefix(' ') else {
        return Parsed::NotACommand;
    };
    if rest.starts_with(char::is_whitespace) {
        return Parsed::NotACommand;
    }
    let (word, args_text) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let Some(name) = Name::from_word(word) else {
        return Parsed::Unknown {
            word: word.to_string(),
        };
    };
    let args = match split_args(args_text) {
        Ok(a) => a,
        Err(message) => return Parsed::Malformed { name, message },
    };
    let max = match name {
        Name::Queue => 1,
        Name::Dequeue | Name::Refresh | Name::Requeue => 0,
        // The arguments of the commands Schlange does not build are not checked.
        _ => usize::MAX,
    };
    if args.len() > max {
        return Parsed::Malformed {
            name,
            message: if max == 0 {
                format!("`{}` takes no argument", name.as_str())
            } else {
                format!(
                    "`{}` takes at most one argument (a queue name)",
                    name.as_str()
                )
            },
        };
    }
    Parsed::Command { name, args }
}

// ---- checkboxes in the queue's own comments -------------------------------------------------

/// Marker of the comment with the "queue this pull request" checkbox.
pub const CONTROLS_MARKER: &str = "<!-- schlange:queue-controls -->";
/// Marker of the status comment.
pub const STATUS_MARKER: &str = "<!-- schlange:status -->";
/// Marks the checkbox line that queues the pull request.
pub const QUEUE_BOX: &str = "<!-- schlange:box:queue -->";
/// Marks the checkbox line that puts a dequeued pull request back.
pub const REQUEUE_BOX: &str = "<!-- schlange:box:requeue -->";

/// A checkbox line, unticked.
pub fn checkbox(label: &str, marker: &str) -> String {
    format!("- [ ] {label} {marker}")
}

/// Whether the checkbox with `marker` is ticked in `body`.
pub fn ticked(body: &str, marker: &str) -> bool {
    body.lines().any(|l| {
        let l = l.trim_start();
        l.contains(marker) && (l.starts_with("- [x]") || l.starts_with("- [X]"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: Name, args: &[&str]) -> Parsed {
        Parsed::Command {
            name,
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn documented_recognition() {
        assert_eq!(parse("@mergifyio queue"), cmd(Name::Queue, &[]));
        assert_eq!(parse("@Mergifyio queue"), cmd(Name::Queue, &[]));
        assert_eq!(parse("@MERGIFY queue"), cmd(Name::Queue, &[]));
        assert_eq!(parse("@mergify rebase"), cmd(Name::Rebase, &[]));
        assert_eq!(
            parse("@mergifyio queue production"),
            cmd(Name::Queue, &["production"])
        );
        assert_eq!(parse("Please @Mergifyio rebase"), Parsed::NotACommand);
        assert_eq!(parse("LGTM\n@mergifyio queue"), Parsed::NotACommand);
        assert_eq!(
            parse("@mergifyio queue\nthis line is left alone"),
            cmd(Name::Queue, &[])
        );
        assert_eq!(
            parse("@mergifyio  queue"),
            Parsed::NotACommand,
            "two spaces"
        );
        assert_eq!(parse("@mergifyio\tqueue"), Parsed::NotACommand);
        assert_eq!(parse("@mergifyiox queue"), Parsed::NotACommand);
        assert_eq!(parse("@mergifyio requeue"), cmd(Name::Requeue, &[]));
        assert_eq!(parse("@mergifyio dequeue"), cmd(Name::Dequeue, &[]));
        assert_eq!(parse("@mergifyio refresh"), cmd(Name::Refresh, &[]));
    }

    #[test]
    fn arguments_and_errors() {
        assert_eq!(
            parse("@mergifyio queue \"hot fixes\""),
            cmd(Name::Queue, &["hot fixes"])
        );
        assert_eq!(
            parse("@mergifyio queue 'a b'  "),
            cmd(Name::Queue, &["a b"])
        );
        assert!(matches!(
            parse("@mergifyio queue a b"),
            Parsed::Malformed {
                name: Name::Queue,
                ..
            }
        ));
        assert!(matches!(
            parse("@mergifyio dequeue now"),
            Parsed::Malformed {
                name: Name::Dequeue,
                ..
            }
        ));
        assert!(matches!(
            parse("@mergifyio queue \"open"),
            Parsed::Malformed { .. }
        ));
        assert_eq!(
            parse("@mergifyio thanks!"),
            Parsed::Unknown {
                word: "thanks!".into()
            }
        );
        assert_eq!(parse("@mergifyio"), Parsed::Unknown { word: "".into() });
        assert_eq!(
            parse("@mergifyio backport stable/1 stable/2"),
            cmd(Name::Backport, &["stable/1", "stable/2"])
        );
        assert_eq!(
            parse("\r\n  @mergifyio queue\r\nmore"),
            cmd(Name::Queue, &[])
        );
        assert_eq!(parse("@mergifyio QUEUE"), cmd(Name::Queue, &[]));
    }

    #[test]
    fn checkboxes() {
        let body = format!("text\n{}\n", checkbox("Queue it", QUEUE_BOX));
        assert!(!ticked(&body, QUEUE_BOX));
        let t = body.replace("- [ ]", "- [x]");
        assert!(ticked(&t, QUEUE_BOX));
        assert!(!ticked(&t, REQUEUE_BOX));
        assert!(ticked(&body.replace("- [ ]", "  - [X]"), QUEUE_BOX));
    }
}
