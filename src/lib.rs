//! musman: fast hard-link manager for multi-format music libraries.

mod clean;
mod link;
mod names;
mod scan;
mod stats;
mod walk;

pub use stats::Stats;
