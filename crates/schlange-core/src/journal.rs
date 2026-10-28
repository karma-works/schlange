//! The journal: what a host persists so the engine survives a crash (MQ-31, KONZEPT.md section
//! 6, "Journal vor jedem Seiteneffekt").
//!
//! The protocol, per engine step:
//!
//! 1. `checkpoint(state)`: the state the step produced, before any of its side effects.
//! 2. For every side effect: `intent(seq, decision)` before it is carried out, `done(seq)`
//!    after the forge answered (or the answer was handed to the engine).
//!
//! After a crash the host calls `recover`: the last checkpoint and the intents without `done`
//! (side effects that may or may not have happened). It restarts the engine with that state,
//! feeds `Event::Restarted` (every outstanding request is in doubt and is retried or resolved
//! from a read) and then a full read of the forge. The engine never needs the intents to be
//! correct: the state already says which requests are outstanding. They are the audit trail
//! of what may have been done, and the host can report them.
//!
//! `MemoryJournal` keeps everything as JSON text in memory, which proves the state round-trips
//! through serialisation. A SQLite journal (one table for checkpoints, one for intents, WAL
//! mode) implements the same trait later.

use std::collections::BTreeMap;

use crate::engine::{Decision, State};

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal serialisation: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("journal storage: {0}")]
    Storage(String),
}

/// What `recover` returns.
#[derive(Debug, Clone)]
pub struct Recovered {
    pub state: State,
    /// Side effects that were about to be carried out and not confirmed, in order.
    pub in_doubt: Vec<(u64, Decision)>,
}

pub trait Journal {
    /// Persists the state an engine step produced, before its side effects.
    fn checkpoint(&mut self, state: &State) -> Result<(), JournalError>;
    /// Records a side effect about to be carried out.
    fn intent(&mut self, seq: u64, decision: &Decision) -> Result<(), JournalError>;
    /// The side effect `seq` was carried out and its answer handed to the engine.
    fn done(&mut self, seq: u64) -> Result<(), JournalError>;
    /// The last checkpoint and the side effects in doubt; `None` for a fresh journal.
    fn recover(&self) -> Result<Option<Recovered>, JournalError>;
}

/// A journal in memory, as JSON text.
#[derive(Debug, Default, Clone)]
pub struct MemoryJournal {
    checkpoint: Option<String>,
    intents: BTreeMap<u64, String>,
    /// Checkpoints written, for reports.
    pub checkpoints: u64,
}

impl Journal for MemoryJournal {
    fn checkpoint(&mut self, state: &State) -> Result<(), JournalError> {
        self.checkpoint = Some(serde_json::to_string(state)?);
        self.checkpoints += 1;
        Ok(())
    }

    fn intent(&mut self, seq: u64, decision: &Decision) -> Result<(), JournalError> {
        self.intents.insert(seq, serde_json::to_string(decision)?);
        Ok(())
    }

    fn done(&mut self, seq: u64) -> Result<(), JournalError> {
        self.intents.remove(&seq);
        Ok(())
    }

    fn recover(&self) -> Result<Option<Recovered>, JournalError> {
        let Some(text) = &self.checkpoint else {
            return Ok(None);
        };
        let state: State = serde_json::from_str(text)?;
        let in_doubt = self
            .intents
            .iter()
            .map(|(seq, d)| Ok((*seq, serde_json::from_str(d)?)))
            .collect::<Result<Vec<_>, JournalError>>()?;
        Ok(Some(Recovered { state, in_doubt }))
    }
}

/// Whether a decision is a side effect on the forge (the others are reports or timers).
pub fn is_side_effect(d: &Decision) -> bool {
    matches!(
        d,
        Decision::BuildCar { .. }
            | Decision::CheckInPlace { .. }
            | Decision::CloseCar { .. }
            | Decision::Refresh { .. }
            | Decision::Land { .. }
            | Decision::Act { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Event, step};
    use crate::model::{CheckRun, Conclusion, PullRequest};
    use schlange_config::rules::Rules;

    #[test]
    fn a_busy_state_round_trips_and_open_intents_are_reported() {
        let rules = Rules::from_yaml(
            "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\n\
             pull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n",
        )
        .unwrap();
        let mut pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
        pr.checks = vec![CheckRun {
            name: "ci".into(),
            app: None,
            head_sha: "h1".into(),
            conclusion: Some(Conclusion::Success),
            seq: 1,
            ..Default::default()
        }];
        let (s, d) = step(
            &State::default(),
            &rules,
            &Event::Snapshot {
                pull_requests: vec![pr],
                branches: [("main".to_string(), Some("b0".to_string()))].into(),
                token: None,
            },
            5,
        );
        let build = d
            .iter()
            .find(|d| matches!(d, Decision::BuildCar { .. }))
            .cloned()
            .expect("a car");
        let mut j = MemoryJournal::default();
        assert!(j.recover().unwrap().is_none());
        j.checkpoint(&s).unwrap();
        j.intent(1, &build).unwrap();
        j.intent(2, &Decision::WakeAt { at: 9 }).unwrap();
        j.done(2).unwrap();
        let r = j.recover().unwrap().unwrap();
        assert_eq!(
            serde_json::to_string(&r.state).unwrap(),
            serde_json::to_string(&s).unwrap()
        );
        assert_eq!(r.in_doubt, vec![(1, build.clone())]);
        assert!(is_side_effect(&build));
    }
}
