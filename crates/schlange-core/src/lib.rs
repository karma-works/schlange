//! The core of Schlange: a deterministic state machine without I/O.
//! `(state, event) → (state, decisions)`; a forge adapter carries out the decisions.

pub mod actions;
pub mod bisect;
pub mod commands;
pub mod control;
pub mod cosmetics;
pub mod engine;
pub mod eval;
pub mod explain;
pub mod gates;
pub mod journal;
pub mod message;
pub mod model;
pub mod protections;
pub mod queue;
pub mod reasons;
pub mod status;
pub mod template;
pub mod time;

pub use engine::{Decision, Event, State, step};
