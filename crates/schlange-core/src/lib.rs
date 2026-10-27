//! The core of Schlange: a deterministic state machine without I/O.
//! `(state, event) → (state, decisions)`; a forge adapter carries out the decisions.

pub mod engine;
pub mod eval;
pub mod model;

pub use engine::{Decision, Event, State, step};
