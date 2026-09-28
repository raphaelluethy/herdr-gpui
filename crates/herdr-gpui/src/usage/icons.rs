//! Provider logos from CodexBar (MIT), monochrome so they tint like the
//! other embedded icons. See `assets/icons/providers/README.md`.

pub(crate) static PROVIDER_ICONS: &[(&str, &[u8])] = &[(
    "icons/providers/grok.svg",
    include_bytes!("../../../../assets/icons/providers/grok.svg"),
)];

/// The logo for a provider id, when one ships.
pub(crate) fn for_id(id: &str) -> Option<&'static str> {
    PROVIDER_ICONS.iter().map(|(path, _)| *path).find(|path| {
        path.strip_prefix("icons/providers/")
            .and_then(|file| file.strip_suffix(".svg"))
            == Some(id)
    })
}
