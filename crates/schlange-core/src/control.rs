//! Running comment commands and checkbox ticks (docs: commands, commands/queue,
//! commands/dequeue, commands/restrictions).
//!
//! - Every comment is handled once (by id and edit version), so a reconciliation can hand over
//!   comments the engine has seen before. A comment on a pull request the engine has not read
//!   yet waits for the next delivery.
//! - `commands_restrictions` are evaluated when the command runs, with the sender, the
//!   sender's permission as the adapter read it with the comment, and `{{author}}` replaced by
//!   the pull request's author.
//! - `queue [name]` leaves a pending command: it enters the queue once the queue conditions
//!   hold (and no protection holds it). A second `queue` with the same or no name is ignored
//!   with a reply; one naming another queue replaces it (and first takes the pull request
//!   out of the queue it is in). A push by anyone but the queue while it waits cancels it,
//!   with a reply naming the pusher; once queued, a push dequeues instead (as before).
//!   `requeue` is the deprecated alias.
//! - `dequeue` takes the pull request out (`PR_MANUALLY_DEQUEUED`) or cancels a pending
//!   command; either way the pull request does not rejoin on its own until a new `queue`.
//!   Otherwise it answers that nothing is queued.
//! - `refresh` reads the pull request again and re-evaluates it.
//! - `backport`, `copy`, `rebase`, `squash` and `update` are recognised and answered as not
//!   supported yet.
//! - Every command is acknowledged with a 👍 reaction. Replies follow the documentation: a
//!   `queue` or `dequeue` that takes effect reports in the status comment, everything else
//!   (refusals, errors, a pull request left out of the queue) gets a reply.

use serde::{Deserialize, Serialize};

use schlange_config::rules::Rules;

use crate::commands::{self, CONTROLS_MARKER, Name, Parsed, QUEUE_BOX, REQUEUE_BOX, STATUS_MARKER};
use crate::cosmetics::{Cosmetic, emit};
use crate::engine::{Decision, DequeueReason, State, context, dequeue};
use crate::eval;
use crate::model::{Comment, Time};
use crate::reasons as code;

/// A `queue` command waiting for the queue conditions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCommand {
    /// The queue it names; `None`: the first queue whose conditions hold.
    pub queue: Option<String>,
    /// Who asked.
    pub by: String,
    /// The comment, `None` for a checkbox.
    pub comment: Option<u64>,
    /// The head it vouches for.
    pub head_sha: String,
    pub since: Time,
}

fn reply(out: &mut Vec<Decision>, number: u64, to: Option<u64>, body: String) {
    emit(out, Cosmetic::Reply { number, to, body });
}

/// Whether `sender` may run `command` on pull request `number`; `Err` with the reason.
fn allowed(
    state: &State,
    rules: &Rules,
    command: &str,
    number: u64,
    sender: &str,
    permission: Option<&str>,
) -> Result<(), String> {
    let pr = &state.pull_requests[&number];
    let conds = eval::with_author(&rules.commands_restrictions.conditions(command), &pr.author);
    let mut ctx = context(state, number);
    ctx.sender = Some(sender.to_string());
    ctx.sender_permission = permission.map(str::to_string);
    let mut errors = Vec::new();
    let (ok, results) = eval::eval_list(&conds, pr, &ctx, &mut errors);
    if ok {
        return Ok(());
    }
    let first = results.iter().position(|r| !r).map_or(0, |i| i + 1);
    let mut why = format!("condition {first} of the `{command}` restriction is not met");
    if let Some(e) = errors.first() {
        why.push_str(&format!(" ({e})"));
    }
    Err(why)
}

/// Handles comments the forge delivered.
pub(crate) fn comments(
    state: &mut State,
    rules: &Rules,
    comments: &[Comment],
    out: &mut Vec<Decision>,
) {
    for c in comments {
        let key = (c.id, c.version.clone());
        if state.seen_comments.contains(&key) || !state.pull_requests.contains_key(&c.number) {
            continue;
        }
        state.seen_comments.insert(key);
        if c.by_queue {
            if c.edited && c.sender != c.author {
                ticked(state, rules, c, out);
            }
            continue;
        }
        if c.edited {
            // Editing a command does not run it again.
            continue;
        }
        let parsed = commands::parse(&c.body);
        if parsed == Parsed::NotACommand {
            continue;
        }
        emit(
            out,
            Cosmetic::React {
                number: c.number,
                comment: c.id,
                content: "+1".into(),
            },
        );
        run(state, rules, c, parsed, out);
    }
}

/// A ticked checkbox in the queue's own comment: `queue` by whoever ticked it.
fn ticked(state: &mut State, rules: &Rules, c: &Comment, out: &mut Vec<Decision>) {
    let queue_box = c.body.contains(CONTROLS_MARKER) && commands::ticked(&c.body, QUEUE_BOX);
    let requeue_box = c.body.contains(STATUS_MARKER) && commands::ticked(&c.body, REQUEUE_BOX);
    if !queue_box && !requeue_box {
        return;
    }
    let n = c.number;
    // Whatever happens, the box is shown unticked again: forget what was last shown so the
    // comment is rewritten.
    let shown = if queue_box {
        &mut state.cosmetics.controls
    } else {
        &mut state.cosmetics.status
    };
    if let Some(b) = shown.get_mut(&n) {
        b.clear();
    }
    if state.queued.contains_key(&n) || state.pending_commands.contains_key(&n) {
        return;
    }
    if let Err(why) = allowed(
        state,
        rules,
        "queue",
        n,
        &c.sender,
        c.sender_permission.as_deref(),
    ) {
        reply(
            out,
            n,
            None,
            format!(
                "@{}, the checkbox did not queue this pull request: {why}.",
                c.sender
            ),
        );
        return;
    }
    queue(state, rules, c, None, None, out);
}

fn run(state: &mut State, rules: &Rules, c: &Comment, parsed: Parsed, out: &mut Vec<Decision>) {
    let n = c.number;
    let to = Some(c.id);
    let sender = c.sender.clone();
    let (name, args) = match parsed {
        Parsed::NotACommand => return,
        Parsed::Unknown { word } => {
            let what = if word.is_empty() {
                "no command was given".to_string()
            } else {
                format!("`{word}` is not a command Schlange knows")
            };
            reply(
                out,
                n,
                to,
                format!(
                    "@{sender}, {what}. The queue understands `queue [name]`, `dequeue` and \
                     `refresh`."
                ),
            );
            return;
        }
        Parsed::Malformed { name, message } => {
            reply(
                out,
                n,
                to,
                format!("@{sender}, `{}` was not run: {message}.", name.as_str()),
            );
            return;
        }
        Parsed::Command { name, args } => (name, args),
    };
    if let Err(why) = allowed(
        state,
        rules,
        name.as_str(),
        n,
        &sender,
        c.sender_permission.as_deref(),
    ) {
        reply(
            out,
            n,
            to,
            format!(
                "@{sender}, you may not run `{}` on this pull request: {why}.",
                name.as_str()
            ),
        );
        return;
    }
    if name.not_built() {
        reply(
            out,
            n,
            to,
            format!(
                "@{sender}, `{}` is a known command, but Schlange does not support it yet; \
                 nothing was done.",
                name.as_str()
            ),
        );
        return;
    }
    match name {
        Name::Queue | Name::Requeue => {
            let target = args.into_iter().next();
            queue(state, rules, c, target, to, out);
        }
        Name::Dequeue => {
            if state.queued.contains_key(&n) {
                state.manual_dequeued.insert(n);
                dequeue(
                    state,
                    rules,
                    n,
                    DequeueReason::new(
                        code::PR_MANUALLY_DEQUEUED,
                        format!("dequeued by @{sender} with a command"),
                    ),
                    out,
                );
            } else if let Some(p) = state.pending_commands.remove(&n) {
                state.manual_dequeued.insert(n);
                reply(
                    out,
                    n,
                    to,
                    format!(
                        "@{sender}, the waiting queue command by @{} is cancelled; this pull \
                         request will not be queued.",
                        p.by
                    ),
                );
            } else {
                reply(
                    out,
                    n,
                    to,
                    format!("@{sender}, this pull request is not queued; nothing to do."),
                );
            }
        }
        Name::Refresh => {
            state.reported_errors.retain(|(x, _)| *x != n);
            state.blocked_attrs.remove(&n);
            let token = state.next_id();
            out.push(Decision::Refresh {
                token,
                numbers: vec![n],
                branches: vec![],
            });
            reply(
                out,
                n,
                to,
                format!("@{sender}, this pull request is read again and re-evaluated."),
            );
        }
        _ => {}
    }
}

/// A `queue` command (or ticked checkbox) that passed its restriction.
fn queue(
    state: &mut State,
    rules: &Rules,
    c: &Comment,
    target: Option<String>,
    to: Option<u64>,
    out: &mut Vec<Decision>,
) {
    let n = c.number;
    let sender = c.sender.clone();
    if let Some(t) = &target
        && !rules.queue_rules.iter().any(|q| &q.name == t)
    {
        reply(
            out,
            n,
            to,
            format!("@{sender}, there is no queue named `{t}`; nothing was queued."),
        );
        return;
    }
    let pr = state.pull_requests[&n].clone();
    if pr.closed || pr.merged {
        reply(
            out,
            n,
            to,
            format!("@{sender}, this pull request is closed; nothing was queued."),
        );
        return;
    }
    if let Some(entry) = state.queued.get(&n).cloned() {
        if target.as_ref().is_none_or(|t| *t == entry.queue) {
            reply(
                out,
                n,
                to,
                format!(
                    "@{sender}, this pull request is already in queue `{}`; the command is \
                     ignored.",
                    entry.queue
                ),
            );
            return;
        }
        dequeue(
            state,
            rules,
            n,
            DequeueReason::assumed(
                code::PR_MANUALLY_DEQUEUED,
                format!(
                    "moved from queue `{}` to `{}` by @{sender}",
                    entry.queue,
                    target.as_deref().unwrap_or("")
                ),
            ),
            out,
        );
    } else if let Some(p) = state.pending_commands.get(&n) {
        if target.is_none() || target == p.queue {
            reply(
                out,
                n,
                to,
                format!(
                    "@{sender}, a queue command by @{} is already waiting for this pull request; \
                     this one is ignored.",
                    p.by
                ),
            );
            return;
        }
        reply(
            out,
            n,
            to,
            format!(
                "@{sender}, the waiting queue command now targets queue `{}`.",
                target.as_deref().unwrap_or("")
            ),
        );
    }
    // The command resets what kept the pull request out: a manual dequeue, a failed head,
    // attributes that failed the merge conditions.
    state.manual_dequeued.remove(&n);
    state.blocked_heads.remove(&n);
    state.blocked_attrs.remove(&n);
    let head_sha = c.head_sha.clone().unwrap_or_else(|| pr.head_sha.clone());
    state.pending_commands.insert(
        n,
        PendingCommand {
            queue: target,
            by: sender,
            comment: to,
            head_sha,
            since: state.now,
        },
    );
}
