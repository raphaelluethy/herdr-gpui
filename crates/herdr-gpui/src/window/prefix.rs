//! Herdr's prefix chords, as the TUI types them: the prefix arms the window,
//! and the next keystroke runs its chord's command or, when nothing is bound
//! to it, is swallowed. Typing the prefix twice sends it on to the focused
//! element as ordinary input, and Escape simply cancels. A chord typed in a
//! menu's text field closes the menu and runs, as it would from the terminal.

use super::HerdrWindow;
use gpui::{Context, Keystroke, Subscription, Window};

impl HerdrWindow {
    /// GPUI calls interceptors before any binding or key handler sees the
    /// keystroke, so an armed prefix claims the next key wherever focus is.
    /// Interceptors are app-wide; each window answers only for itself.
    pub(crate) fn intercept_prefix(window: &Window, cx: &mut Context<Self>) -> Subscription {
        let own = window.window_handle().window_id();
        let view = cx.weak_entity();
        cx.intercept_keystrokes(move |event, window, cx| {
            if window.window_handle().window_id() != own {
                return;
            }
            let _ = view.update(cx, |this, cx| {
                this.prefix_keystroke(&event.keystroke, window, cx);
            });
        })
    }

    fn prefix_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keymap = &self.config.keybindings;
        if !self.prefix_armed {
            if keymap.is_prefix(keystroke) {
                self.prefix_armed = true;
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        self.prefix_armed = false;
        cx.notify();
        if keymap.is_prefix(keystroke) {
            return;
        }
        cx.stop_propagation();
        let Some(command) = keymap.chord(keystroke) else {
            return;
        };
        // Commands wait while a menu page holds input, so the chord ends it
        // first. Run directly rather than through focus, which the menu held.
        if self.menu.page.is_some() {
            self.dismiss_menu(window, cx);
        }
        self.command(command, window, cx);
    }

    /// Leaving the window abandons a half-typed chord, as the TUI's prefix
    /// mode does not outlive its client.
    pub(super) fn disarm_prefix(&mut self) {
        self.prefix_armed = false;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::keymap::{DaemonKeys, Keymap};
    use gpui::{Keystroke, TestAppContext, VisualTestContext};

    /// Whether something handled the keystroke. Nothing binds `cmd-y` and
    /// the terminal ignores cmd keys, so only the prefix can claim it.
    fn press(keystroke: &str, cx: &mut VisualTestContext) -> bool {
        let handled = cx.update(|window, cx| {
            window.dispatch_keystroke(Keystroke::parse(keystroke).unwrap(), cx)
        });
        cx.run_until_parked();
        handled
    }

    #[gpui::test]
    fn the_prefix_runs_chords_and_swallows_other_keys(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        let table: toml::Table = "prefix = 'cmd+j'\ntoggle_sidebar = 'prefix+cmd+b'"
            .parse()
            .unwrap();
        view.update(cx, |view, _| {
            view.config.keybindings =
                Keymap::with_overrides(&Default::default(), &DaemonKeys::from_table(Some(&table)))
                    .unwrap();
        });
        let state = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| (view.prefix_armed, view.sidebar_visible))
        };
        cx.update(|window, cx| {
            window.focus(&view.read(cx).focus.clone(), cx);
            window.draw(cx).clear(cx);
        });
        assert!(!press("cmd-y", cx));
        assert!(
            !press("cmd-b", cx),
            "the chord's key alone is not a binding"
        );
        assert_eq!(state(cx), (false, true));

        assert!(press("cmd-j", cx));
        assert_eq!(state(cx), (true, true));
        assert!(press("cmd-b", cx));
        assert_eq!(state(cx), (false, false), "the chord ran");

        // An unbound key after the prefix goes nowhere, as in the TUI.
        assert!(press("cmd-j", cx));
        assert!(press("cmd-y", cx));
        assert_eq!(state(cx), (false, false));
        assert!(!press("cmd-y", cx), "only the next key is claimed");

        // The prefix twice passes the second one through.
        assert!(press("cmd-j", cx));
        assert!(!press("cmd-j", cx));
        assert_eq!(state(cx), (false, false));

        assert!(press("cmd-j", cx));
        assert!(press("escape", cx));
        assert_eq!(state(cx), (false, false));
        assert!(press("cmd-j", cx));
        assert!(press("cmd-b", cx));
        assert_eq!(state(cx), (false, true));

        // From a menu's focused text field, the chord closes the menu and runs.
        view.update_in(cx, |view, window, cx| view.open_keybinds(window, cx));
        cx.run_until_parked();
        let search_focused = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let view = view.read(cx);
                view.menu.page.is_some()
                    && view
                        .menu
                        .keybinds_search
                        .as_ref()
                        .is_some_and(|search| search.read(cx).focus.is_focused(window))
            })
        };
        assert!(search_focused(cx));
        assert!(press("cmd-j", cx));
        assert!(search_focused(cx), "the prefix alone leaves the menu open");
        assert!(press("cmd-b", cx));
        assert_eq!(state(cx), (false, false));
        view.read_with(cx, |view, _| assert!(view.menu.page.is_none()));

        // An unbound key there is swallowed and leaves the menu open.
        view.update_in(cx, |view, window, cx| view.open_keybinds(window, cx));
        cx.run_until_parked();
        assert!(press("cmd-j", cx));
        assert!(press("cmd-y", cx));
        assert!(search_focused(cx));
    }
}
