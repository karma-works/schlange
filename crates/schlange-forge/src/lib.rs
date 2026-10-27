//! The forge interface and its implementations. Phase 0 has the simulated forge (`sim`);
//! `localgit`, `forgejo` and `github` follow (KONZEPT.md, section 6).

pub mod scenario;
pub mod sim;

use schlange_core::model::{PullRequest, Time};
use schlange_core::{Decision, Event};

#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    #[error("pull request #{0} does not exist")]
    UnknownPullRequest(u64),
}

/// A code forge as the engine sees it. Webhooks are hints; `pull_request` is the
/// reconciliation read that is the truth (KONZEPT.md, section 6).
pub trait Forge {
    /// Current time of the forge (virtual in the simulation).
    fn now(&self) -> Time;
    /// The next event the forge delivers, advancing time to it. `None` when nothing is pending.
    fn next_event(&mut self) -> Option<(Time, Event)>;
    /// Reads the current state of a pull request.
    fn pull_request(&self, number: u64) -> Option<PullRequest>;
    /// Carries out one decision of the engine.
    fn execute(&mut self, decision: &Decision) -> Result<(), ForgeError>;
}
