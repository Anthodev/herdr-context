//! Stable, backend-neutral contracts for `herdr-context`.
//!
//! Transport, VCS, and transcript adapters belong behind these contracts. Domain
//! consumers should never need Herdr wire records, Git/Jujutsu output, or a closed
//! list of conversation tools.

fn normalize_nonempty(value: impl Into<String>) -> Option<String> {
    let value = value.into();
    let normalized_len = value.trim().len();
    if normalized_len == 0 {
        return None;
    }
    if normalized_len == value.len() {
        return Some(value);
    }
    Some(value.trim().to_owned())
}

pub mod app;
pub mod config;
mod controller;
pub mod conversations;
pub mod files;
pub mod host;
pub mod input;
pub mod intent;
pub mod model;
#[cfg(feature = "perf-harness")]
pub mod perf;
pub mod project;
pub mod runtime;
pub mod ui;
pub mod vcs;
pub mod worker;
