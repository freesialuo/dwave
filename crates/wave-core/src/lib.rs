//! Synchronous, read-only client and local SQLite cache for Drips Wave.
//!
//! `wave-core` powers the `dwave` CLI with:
//! - [`source::HttpWaveSource`] and [`gateway::Gateway`] for rate-limited, budgeted, retry-safe Wave GET requests,
//! - [`store::Store`] and [`engine::Engine`] for TTL-aware caching, query planning, and resumable scoped syncs,
//! - [`watch::Watcher`] for on-demand watchlist snapshots and change detection,
//! - [`github::GitHubClient`] for conditional (`ETag`/`304`) GitHub issue and timeline cross-checking, and
//! - tolerant typed projections ([`Issue`], [`IssueState`], [`Program`], [`Repo`], [`Wave`], [`WaveStatus`]) that preserve unrecognized fields in `extra`.

pub mod config;
pub mod engine;
pub mod filter;
pub mod gateway;
pub mod github;
pub mod models;
pub mod source;
pub mod store;
pub mod watch;

pub use models::{Issue, IssueState, Program, Repo, Wave, WaveStatus};
