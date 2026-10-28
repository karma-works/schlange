//! Configuration of Schlange: reading `.mergify.yml`, validating it against the pinned Mergify
//! schema, parsing conditions, and the feature map with its support matrix.
//!
//! Validation stages (KONZEPT.md, section 5):
//! 1. YAML with positions ([`yaml`]),
//! 2. Mergify schema ([`schema`]),
//! 3. conditions ([`conditions`], checked in [`validate`]),
//! 4. semantics ([`semantics`]),
//! 5. support by Schlange ([`support`], from [`features`]).
//!
//! [`rules`] gives the engine a typed view of queue rules and pull request rules.

pub mod actions;
pub mod conditions;
pub mod config_file;
pub mod extends;
pub mod features;
pub mod glob;
pub mod rules;
pub mod schema;
pub mod semantics;
pub mod support;
pub mod validate;
pub mod yaml;

pub use features::FeatureMap;
pub use schema::Schema;
pub use validate::{Report, validate};
