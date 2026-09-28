//! Every provider, in the order the status bar and panel list them.
//! Adding one is a module in `providers/` and a line here.

use super::{model::Provider, providers, service::Service};

static SERVICES: &[&dyn Service] = &[
    &providers::codex::Codex,
    &providers::claude::Claude,
    &providers::grok::Grok,
];

pub(crate) fn all() -> impl Iterator<Item = Provider> {
    SERVICES.iter().map(|service| Provider(*service))
}

pub(crate) fn find(id: &str) -> Option<Provider> {
    all().find(|provider| provider.id() == id)
}

/// Where `provider` sits in the list, for ordering readings.
pub(crate) fn position(provider: Provider) -> usize {
    SERVICES
        .iter()
        .position(|service| service.meta().id == provider.id())
        .unwrap_or(SERVICES.len())
}
