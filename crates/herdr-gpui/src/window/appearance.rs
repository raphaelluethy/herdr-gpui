//! Light or dark mode for the app and for the programs in its terminals. The
//! daemon answers a program's color queries (OSC 10/11/4) and color-scheme
//! reports (DSR 996, mode 2031) from what its foreground client reported, so
//! each connection reports the theme it paints with and the mode in effect,
//! which the `appearance` setting can force regardless of the system's.

use super::HerdrWindow;
use crate::{
    app::InitialAppearance,
    config::{Appearance, Config, Theme},
};
use gpui::{App, Context, Window, WindowAppearance};
use herdr_client::protocol::{
    ClientHostAppearance, ClientHostColor, ClientHostDefaultColorKind, ClientHostThemeUpdate,
};

/// What a connection last reported to the daemon: whether it is dark, and
/// the theme whose colors it answers with.
pub(crate) type ReportedTheme = (bool, Theme);

impl HerdrWindow {
    /// Whether the app is in dark mode: the forced choice, else the system's.
    pub(crate) fn is_dark(&self, window: &Window) -> bool {
        self.config.appearance.is_dark(|| {
            matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            )
        })
    }

    /// Tells the daemon which colors and mode this client shows, whenever
    /// either changes or a connection that has not heard them takes over.
    pub(crate) fn report_host_theme(&mut self, window: &Window) {
        let dark = self.is_dark(window);
        if self
            .sent_host_theme
            .as_ref()
            .is_some_and(|(sent, theme)| *sent == dark && *theme == self.theme)
        {
            return;
        }
        let (Some(handle), Some(snapshot)) = (
            &self.endpoints[self.selected_endpoint].connection.handle,
            &self.live.snapshot,
        ) else {
            return;
        };
        // A full queue leaves the report unsent, so the next tick retries it
        // whole; the daemon ignores the parts it already has.
        if host_theme_updates(&self.theme, dark)
            .into_iter()
            .all(|update| handle.set_host_theme(&snapshot.boot_id, update).is_ok())
        {
            self.sent_host_theme = Some((dark, self.theme.clone()));
        }
    }

    /// Switches the appearance here at once, then keeps it: the choice is
    /// saved to the local overrides off the UI thread, and the config watcher
    /// brings every other window along.
    pub(crate) fn set_appearance(&mut self, appearance: Appearance, cx: &mut Context<Self>) {
        self.set_appearance_with(appearance, Config::save_appearance, cx);
    }

    /// `save` persists the choice; tests pass one that leaves the real
    /// config alone.
    pub(crate) fn set_appearance_with(
        &mut self,
        appearance: Appearance,
        save: impl FnOnce(Appearance) -> crate::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.config.appearance == appearance {
            return;
        }
        self.config.appearance = appearance;
        // The menu reads its checkmark from the latest config.
        if cx.has_global::<InitialAppearance>() {
            cx.global_mut::<InitialAppearance>().config.appearance = appearance;
        }
        apply_native_appearance(appearance, cx);
        crate::menus::install(cx);
        cx.notify();
        let save = cx
            .background_executor()
            .spawn(async move { save(appearance) });
        cx.spawn(async move |_, _| {
            if let Err(error) = save.await {
                tracing::warn!(%error, "Could not save the appearance");
            }
        })
        .detach();
    }
}

/// Forces the native window frame, and on macOS the web views and dialogs, to
/// the chosen mode, or lets them follow the system again. GPUI only does this
/// on macOS; elsewhere the app draws its own frame from the theme.
pub(crate) fn apply_native_appearance(appearance: Appearance, cx: &App) {
    cx.set_window_appearance(match appearance {
        Appearance::System => None,
        Appearance::Light => Some(WindowAppearance::Light),
        Appearance::Dark => Some(WindowAppearance::Dark),
    });
}

fn host_color(rgb: u32) -> ClientHostColor {
    let [_, r, g, b] = rgb.to_be_bytes();
    ClientHostColor { r, g, b }
}

/// The daemon's host theme for `theme` in the given mode. The mode goes first
/// and is always explicit, so the daemon never infers it from the background
/// in between.
fn host_theme_updates(theme: &Theme, dark: bool) -> [ClientHostThemeUpdate; 4] {
    [
        ClientHostThemeUpdate::Appearance(if dark {
            ClientHostAppearance::Dark
        } else {
            ClientHostAppearance::Light
        }),
        ClientHostThemeUpdate::DefaultColor {
            kind: ClientHostDefaultColorKind::Foreground,
            color: host_color(theme.foreground),
        },
        ClientHostThemeUpdate::DefaultColor {
            kind: ClientHostDefaultColorKind::Background,
            color: host_color(theme.background),
        },
        ClientHostThemeUpdate::PaletteColors(
            (0..=u8::MAX)
                .zip(theme.palette)
                .map(|(index, rgb)| (index, host_color(rgb)))
                .collect(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_theme_reports_the_mode_first_then_every_painted_color() {
        let theme = Theme::builtin("Catppuccin Latte").unwrap();
        for dark in [false, true] {
            let [appearance, foreground, background, palette] = host_theme_updates(&theme, dark);
            assert_eq!(
                appearance,
                ClientHostThemeUpdate::Appearance(if dark {
                    ClientHostAppearance::Dark
                } else {
                    ClientHostAppearance::Light
                })
            );
            assert_eq!(
                foreground,
                ClientHostThemeUpdate::DefaultColor {
                    kind: ClientHostDefaultColorKind::Foreground,
                    color: ClientHostColor {
                        r: 0x4c,
                        g: 0x4f,
                        b: 0x69
                    },
                }
            );
            assert_eq!(
                background,
                ClientHostThemeUpdate::DefaultColor {
                    kind: ClientHostDefaultColorKind::Background,
                    color: ClientHostColor {
                        r: 0xef,
                        g: 0xf1,
                        b: 0xf5
                    },
                }
            );
            let ClientHostThemeUpdate::PaletteColors(colors) = palette else {
                panic!("the palette comes last");
            };
            // Exactly one entry per index: the daemon drops a client that
            // sends more than a palette holds.
            assert_eq!(colors.len(), 256);
            assert!(colors.iter().enumerate().all(|(i, (index, color))| {
                usize::from(*index) == i && *color == host_color(theme.palette[i])
            }));
        }
    }
}
