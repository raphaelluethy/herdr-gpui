//! Read-only PR prefetch. One worker, bounded memory, and no UI-thread I/O.

mod cache;
mod fetch;
mod lookup;
mod model;
mod parse;

#[cfg(test)]
mod tests;

pub(crate) use {
    cache::Cache,
    fetch::{Output, local_checkout, origin_remote, run, run_split, strip_git_environment},
    lookup::Lookup,
    model::{Input, Origin, PullRequest, State, clean, repository_input},
};

pub(crate) use fetch::local_repository;
#[cfg(any(test, all(feature = "integration-test", target_os = "macos")))]
pub(crate) use model::fixture;

use fetch::{Origins, fetch_with_backoff};
use parse::parse_graphql;

/// Absence is a successful lookup with no PR, not an error.
type Result = crate::Result<Option<PullRequest>>;
