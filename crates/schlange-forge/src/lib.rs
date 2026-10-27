//! The forge side of Schlange. Phase 1a has the simulated forge (`sim`) with an in-memory git
//! (`git`), scripted CI (`ci`), webhook fault injection and reconciliation, the invariant
//! checker (`invariants`), the scenario runner (`runner`), its YAML frontend (`scenario`) and a
//! Rust DSL for tests (`dsl`). Phase 1b adds `localgit` (feature `localgit`): the same forge
//! logic on real git repositories on disk, with real merges, `git update-ref` compare-and-swap
//! and every check as a process in a checkout. `forgejo` and `github` follow (KONZEPT.md,
//! section 6).

pub mod ci;
pub mod dsl;
pub mod git;
pub mod invariants;
#[cfg(feature = "localgit")]
pub mod localgit;
pub mod runner;
pub mod scenario;
pub mod sim;

pub use runner::{RunReport, Scenario, run};

/// Runs a scenario file on the `localgit` forge; an error when this build has no `localgit`
/// (cargo feature) or git is not usable.
pub fn run_file_localgit(path: &std::path::Path) -> Result<RunReport, String> {
    #[cfg(feature = "localgit")]
    {
        localgit::run_file(path).map_err(|e| e.to_string())
    }
    #[cfg(not(feature = "localgit"))]
    {
        let _ = path;
        Err("this build has no `localgit` forge (cargo feature `localgit`)".into())
    }
}

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

impl<G: git::GitStore> Forge for sim::SimForge<G> {
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
