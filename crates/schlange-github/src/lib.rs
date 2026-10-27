//! The GitHub side of Schlange (phase 1c): a GitHub App client over HTTPS with rustls
//! ([`client`], [`api`], [`jwt`]), webhooks ([`webhook`]), the forge adapter
//! ([`forge::GithubForge`]), the `serve` loop with reconciliation ([`service`]), the doctor
//! checks ([`doctor`]) and the least-privilege permission list ([`permissions`]).
//!
//! Everything is blocking (threads, no async runtime): a merge queue sends a few requests per
//! second, and the static binary stays small. The tests drive all of it through HTTP against a
//! fake GitHub (crate `schlange-github-fake`).

pub mod api;
pub mod client;
pub mod doctor;
pub mod forge;
pub mod jwt;
pub mod permissions;
pub mod service;
pub mod time;
pub mod webhook;

pub use api::RepoName;
pub use client::{Client, ClientConfig};
pub use forge::GithubForge;
pub use service::{ConfigSource, ServeConfig, Service};
