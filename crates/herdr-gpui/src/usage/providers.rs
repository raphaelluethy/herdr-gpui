//! One module per provider, each a unit struct implementing
//! [`Service`](super::service::Service). Ported from CodexBar
//! (<https://github.com/steipete/CodexBar>, MIT), which documents how each
//! service signs in and reports usage.

pub(super) mod claude;
pub(super) mod codex;
pub(super) mod grok;
