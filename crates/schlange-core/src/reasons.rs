//! Why a pull request's queue checks ended, with Mergify's codes (`queue-dequeue-reason`).
//!
//! The codes are the schema's enum. The mapping from a situation to a code is ours, from the
//! public documentation. Where the documentation does not say which code a path uses, the
//! reason is marked `assumed` and the feature file `queue-dequeue-reasons.md` lists it.

use serde::Serialize;

pub const PR_DEQUEUED: &str = "PR_DEQUEUED";
pub const PR_AHEAD_DEQUEUED: &str = "PR_AHEAD_DEQUEUED";
pub const BATCH_AHEAD_FAILED: &str = "BATCH_AHEAD_FAILED";
pub const CHECKS_TIMEOUT: &str = "CHECKS_TIMEOUT";
pub const CHECKS_FAILED: &str = "CHECKS_FAILED";
pub const CHECKS_INTERRUPTED: &str = "CHECKS_INTERRUPTED";
pub const DROPPED_BY_BISECTION_ELIMINATION: &str = "DROPPED_BY_BISECTION_ELIMINATION";
pub const QUEUE_RULE_MISSING: &str = "QUEUE_RULE_MISSING";
pub const BASE_BRANCH_CHANGED: &str = "BASE_BRANCH_CHANGED";
pub const PR_UNEXPECTEDLY_FAILED_TO_MERGE: &str = "PR_UNEXPECTEDLY_FAILED_TO_MERGE";
pub const BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS: &str = "BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS";
pub const CONFLICT_WITH_BASE_BRANCH: &str = "CONFLICT_WITH_BASE_BRANCH";
pub const CONFLICT_WITH_PULL_AHEAD: &str = "CONFLICT_WITH_PULL_AHEAD";
pub const DRAFT_PULL_REQUEST_CHANGED: &str = "DRAFT_PULL_REQUEST_CHANGED";
pub const BATCH_PULL_REQUEST_CLOSED: &str = "BATCH_PULL_REQUEST_CLOSED";
pub const PULL_REQUEST_UPDATED: &str = "PULL_REQUEST_UPDATED";
pub const MERGE_QUEUE_RESET: &str = "MERGE_QUEUE_RESET";
pub const PR_MANUALLY_MERGED: &str = "PR_MANUALLY_MERGED";
pub const BATCH_AHEAD_BRANCH_EDITED: &str = "BATCH_AHEAD_BRANCH_EDITED";
pub const BATCH_REBUILT: &str = "BATCH_REBUILT";

/// Why a pull request left the queue, or why its checks ended while it stays queued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DequeueReason {
    /// Mergify's code; `None` when no code fits.
    pub code: Option<&'static str>,
    pub text: String,
    /// The documentation does not say which code Mergify uses here; this one is our choice.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub assumed: bool,
}

impl DequeueReason {
    pub fn new(code: &'static str, text: impl Into<String>) -> Self {
        DequeueReason {
            code: Some(code),
            text: text.into(),
            assumed: false,
        }
    }

    pub fn assumed(code: &'static str, text: impl Into<String>) -> Self {
        DequeueReason {
            assumed: true,
            ..Self::new(code, text)
        }
    }

    pub fn describe(&self) -> String {
        match self.code {
            Some(c) if self.assumed => format!("{c} (code assumed): {}", self.text),
            Some(c) => format!("{c}: {}", self.text),
            None => self.text.clone(),
        }
    }
}

/// Codes after which the `queue` action does not queue the same head again: the pull request
/// has to change first (or be queued by command, which is not built yet).
pub fn blocks_requeue(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            CHECKS_FAILED
                | CHECKS_TIMEOUT
                | CHECKS_INTERRUPTED
                | CONFLICT_WITH_BASE_BRANCH
                | BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS
                | DROPPED_BY_BISECTION_ELIMINATION
                | DRAFT_PULL_REQUEST_CHANGED
                | PR_UNEXPECTEDLY_FAILED_TO_MERGE
        )
    )
}
