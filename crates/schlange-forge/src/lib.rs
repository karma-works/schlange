//! The forge side of Schlange. Phase 1a has the simulated forge (`sim`) with an in-memory git
//! (`git`), scripted CI (`ci`), webhook fault injection and reconciliation, the invariant
//! checker (`invariants`), the scenario runner (`runner`), its YAML frontend (`scenario`) and a
//! Rust DSL for tests (`dsl`). `localgit`, `forgejo` and `github` follow (KONZEPT.md,
//! section 6).

pub mod ci;
pub mod dsl;
pub mod git;
pub mod invariants;
pub mod runner;
pub mod scenario;
pub mod sim;

pub use runner::{RunReport, Scenario, run};

use schlange_core::model::{PullRequest, Time};
use schlange_core::{Decision, Event};

/// A code forge as the engine's adapter sees it. Webhooks are hints; the reads are the truth
/// (KONZEPT.md, section 6). `execute` returns the forge's replies as events.
pub trait Forge {
    fn now(&self) -> Time;
    fn read_pull_request(&self, number: u64) -> Option<PullRequest>;
    fn read_branch(&self, name: &str) -> Option<String>;
    fn execute(&mut self, decision: &Decision) -> Vec<Event>;
}

impl Forge for sim::SimForge {
    fn now(&self) -> Time {
        sim::SimForge::now(self)
    }

    fn read_pull_request(&self, number: u64) -> Option<PullRequest> {
        sim::SimForge::read_pull_request(self, number)
    }

    fn read_branch(&self, name: &str) -> Option<String> {
        sim::SimForge::read_branch(self, name)
    }

    fn execute(&mut self, decision: &Decision) -> Vec<Event> {
        sim::SimForge::execute(self, decision)
    }
}
