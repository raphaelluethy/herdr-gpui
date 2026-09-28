use crate::{
    HerdrWindow,
    config::{Theme, ThemeKey},
    menu::Page,
    search_input::SearchInput,
};
use gpui::{prelude::*, *};

/// Which theme the picker chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeTarget {
    /// The theme on screen: the current mode's half of a pair, or the single
    /// theme, which choosing replaces for both modes.
    Current,
    /// One mode's own setting, `light_theme` or `dark_theme`, so the other
    /// mode keeps what it had.
    Mode { dark: bool },
}

impl ThemeTarget {
    /// The mode this chooses for, given whether the app is dark, and whether
    /// it saves that mode's own setting.
    fn mode(self, app_dark: bool) -> (bool, bool) {
        match self {
            Self::Current => (app_dark, false),
            Self::Mode { dark } => (dark, true),
        }
    }
}

pub(super) struct ThemePicker {
    pub search: Entity<SearchInput>,
    target: ThemeTarget,
    names: Vec<String>,
    pub(super) filtered: Vec<String>,
    selected: usize,
    scroll: UniformListScrollHandle,
    error: Option<String>,
    query: String,
    discovering: bool,
    baseline: Option<Theme>,
    session: u64,
    request: u64,
    desired: Option<String>,
    loaded: Option<String>,
    accepting: bool,
    saving: bool,
    /// The theme setting being saved, and its value.
    saving_as: Option<(ThemeKey, String)>,
    // Keep the slot across dismiss/reopen: blocking I/O cannot be cancelled by
    // dropping a GPUI task. Only its completion may release the slot.
    in_flight: Option<(u64, u64)>,
    window: AnyWindowHandle,
    _subscription: Subscription,
}

impl ThemePicker {
    #[cfg(test)]
    pub(crate) fn target(&self) -> ThemeTarget {
        self.target
    }

    fn filter(&mut self, query: &str) {
        self.query = query.to_owned();
        let query = query.trim().to_lowercase();
        self.filtered = self
            .names
            .iter()
            .filter(|name| name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }
}

impl HerdrWindow {
    pub(super) fn open_theme_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_theme_picker_for(ThemeTarget::Current, window, cx);
    }

    pub(super) fn open_theme_picker_for(
        &mut self,
        target: ThemeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.open_menu(window, cx) {
            return;
        }
        // A pending reload must not replace this newer interactive appearance.
        self.config_load = None;
        self.flush_font_sizes(cx);
        self.menu.page = Some(Page::Themes);
        let mut picker = if let Some(picker) = self.menu.themes.take() {
            picker.search.update(cx, |input, cx| input.clear(cx));
            picker
        } else {
            let search = cx.new(SearchInput::new);
            let subscription = cx.subscribe(
                &search,
                |this, search, _: &crate::search_input::Changed, cx| {
                    if let Some(picker) = &mut this.menu.themes {
                        if picker.accepting || picker.query == search.read(cx).text() {
                            return;
                        }
                        picker.filter(search.read(cx).text());
                    }
                    this.preview_picker_selection(cx);
                },
            );
            ThemePicker {
                search,
                target,
                names: Vec::new(),
                filtered: Vec::new(),
                selected: 0,
                scroll: UniformListScrollHandle::new(),
                error: None,
                query: String::new(),
                discovering: false,
                baseline: None,
                session: 0,
                request: 0,
                desired: None,
                loaded: None,
                accepting: false,
                saving: false,
                saving_as: None,
                in_flight: None,
                window: window.window_handle(),
                _subscription: subscription,
            }
        };
        picker.session += 1;
        picker.target = target;
        picker.baseline = Some(self.theme.clone());
        picker.desired = None;
        picker.loaded = None;
        picker.accepting = false;
        picker.error = None;
        picker.names = Theme::BUILTIN_NAMES
            .iter()
            .map(|name| (*name).into())
            .collect();
        let current = self.theme_name_for(target.mode(self.dark).0);
        if !picker.names.contains(&current) {
            picker.names.push(current.clone());
        }
        picker.names.sort();
        picker.filter("");
        picker.selected = picker
            .filtered
            .iter()
            .position(|name| name == &current)
            .unwrap_or(0);
        picker.search.update(cx, |input, cx| {
            input.set_appearance(self.config.ui.clone(), self.theme.clone(), cx);
            window.focus(&input.focus, cx);
        });
        self.menu.themes = Some(picker);
        self.discover_picker_themes(cx);
        cx.notify();
    }

    fn discover_picker_themes(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        if picker.discovering {
            return;
        }
        picker.discovering = true;
        let session = picker.session;
        let config = self.config.clone();
        let discovery = cx
            .background_executor()
            .spawn(async move { config.available_themes() });
        cx.spawn(async move |this, cx| {
            let result = discovery.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_picker_discovery(session, result, cx);
            });
        })
        .detach();
    }

    fn finish_picker_discovery(
        &mut self,
        session: u64,
        result: crate::Result<Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        let current = self.current_theme_name();
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        picker.discovering = false;
        if picker.baseline.is_none() {
            return;
        }
        match result {
            Ok(mut names) => {
                // Theme directories are process-wide, not picker-session state.
                // Reuse a running scan on reopen, adding the current explicit selection.
                if !names.contains(&current) {
                    names.push(current);
                    names.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
                }
                let selected = picker.filtered.get(picker.selected).cloned();
                picker.names = names;
                picker.filter(picker.search.read(cx).text());
                if let Some(index) = picker
                    .filtered
                    .iter()
                    .position(|name| Some(name) == selected.as_ref())
                {
                    picker.selected = index;
                }
            }
            Err(error) if picker.session == session => picker.error = Some(error.to_string()),
            Err(_) => {}
        }
        if !picker.query.is_empty() && picker.desired.is_none() {
            self.preview_picker_selection(cx);
        }
        cx.notify();
    }

    /// The open picker's mode and split, as [`ThemeTarget::mode`].
    fn picker_mode(&self) -> (bool, bool) {
        let target = self
            .menu
            .themes
            .as_ref()
            .map_or(ThemeTarget::Current, |picker| picker.target);
        target.mode(self.dark)
    }

    /// The theme a mode uses: its half of a light/dark pair, or the single one.
    pub(crate) fn theme_name_for(&self, dark: bool) -> String {
        self.config.theme_names().map_or_else(
            |_| self.config.theme.clone(),
            |names| names.pick(dark).to_owned(),
        )
    }

    /// The theme the open picker lists as current and replaces.
    fn current_theme_name(&self) -> String {
        self.theme_name_for(self.picker_mode().0)
    }

    /// Points an open preview's cancel at `theme`, for a mode switch while
    /// the picker is open. Returns whether a preview was open.
    pub(super) fn retarget_theme_preview(&mut self, theme: &Theme) -> bool {
        let Some(baseline) = self
            .menu
            .themes
            .as_mut()
            .and_then(|picker| picker.baseline.as_mut())
        else {
            return false;
        };
        *baseline = theme.clone();
        true
    }

    pub(super) fn theme_save_in_flight(&self) -> bool {
        self.menu
            .themes
            .as_ref()
            .is_some_and(|picker| picker.saving)
    }

    pub(super) fn cancel_theme_preview(&mut self, cx: &mut Context<Self>) -> bool {
        // Starting the disk write is the commit boundary. Its result must be
        // reconciled before another modal, cancellation, or reload can proceed.
        if self.theme_save_in_flight() {
            return false;
        }
        if let Some(picker) = &mut self.menu.themes {
            if let Some(theme) = picker.baseline.take() {
                self.theme = theme;
                crate::log_window::set_appearance(&self.config, &self.theme, cx);
            }
            picker.session += 1;
            picker.desired = None;
            picker.accepting = false;
        }
        true
    }

    fn preview_picker_selection(&mut self, cx: &mut Context<Self>) {
        if self.menu.page != Some(Page::Themes) {
            return;
        }
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        if picker.accepting {
            return;
        }
        let desired = picker.filtered.get(picker.selected).cloned();
        if picker.desired != desired {
            picker.request += 1;
            picker.desired = desired;
            picker.loaded = None;
            picker.error = None;
            if let Some(theme) = picker
                .desired
                .as_deref()
                .and_then(|name| Theme::builtin(name.trim()))
            {
                self.theme = theme;
                picker.loaded = picker.desired.clone();
            } else if picker.desired.is_none()
                && let Some(theme) = &picker.baseline
            {
                self.theme = theme.clone();
            }
        }
        picker.search.update(cx, |input, cx| {
            input.set_appearance(self.config.ui.clone(), self.theme.clone(), cx)
        });
        crate::log_window::set_appearance(&self.config, &self.theme, cx);
        self.drive_picker_load(cx);
        cx.notify();
    }

    fn apply_picker_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        if picker.accepting {
            return;
        }
        if let Some(index) = picker
            .filtered
            .iter()
            .position(|candidate| candidate == name)
        {
            picker.selected = index;
        } else {
            return;
        }
        self.preview_picker_selection(cx);
        if let Some(picker) = &mut self.menu.themes {
            picker.accepting = true;
        }
        self.drive_picker_load(cx);
    }

    fn drive_picker_load(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        if picker.baseline.is_none() || picker.in_flight.is_some() {
            return;
        }
        let Some(name) = picker.desired.clone() else {
            return;
        };
        if picker.loaded.as_ref() == Some(&name) && !picker.accepting {
            return;
        }
        let token = (picker.session, picker.request);
        let saving = picker.accepting && picker.loaded.as_ref() == Some(&name);
        picker.saving = saving;
        picker.in_flight = Some(token);
        let window = picker.window;
        let (dark, own) = picker.target.mode(self.dark);
        let choice = if saving {
            // Recorded now, so a mode switch while the save runs still
            // updates the setting that was chosen.
            let choice = self.config.theme_choice(dark, &name, own);
            picker.saving_as = choice.as_ref().ok().cloned();
            Some(choice)
        } else {
            None
        };
        let mut config = self.config.clone();
        config.theme = name;
        let theme = self.theme.clone();
        let task = cx.background_executor().spawn(async move {
            match choice {
                Some(choice) => {
                    let (key, value) = choice?;
                    config.save_theme(key, &value).map(|()| theme)
                }
                None => config.theme(),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = window.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.finish_picker_load(token, saving, result, window, cx)
                });
            });
        })
        .detach();
    }

    fn finish_picker_load(
        &mut self,
        token: (u64, u64),
        saving: bool,
        result: crate::Result<Theme>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        if picker.in_flight != Some(token) {
            return;
        }
        picker.in_flight = None;
        picker.saving = false;
        let saved_as = picker.saving_as.take();
        if picker.baseline.is_none() || token != (picker.session, picker.request) {
            self.drive_picker_load(cx);
            return;
        }
        match result {
            Ok(theme) => {
                self.theme = theme;
                picker.loaded = picker.desired.clone();
                picker.search.update(cx, |input, cx| {
                    input.set_appearance(self.config.ui.clone(), self.theme.clone(), cx)
                });
                if saving {
                    let (dark, own) = picker.target.mode(self.dark);
                    let name = picker.desired.clone().unwrap_or_default();
                    let saved_as =
                        saved_as.or_else(|| self.config.theme_choice(dark, &name, own).ok());
                    if let Some((key, value)) = saved_as {
                        self.config.set_theme(key, &value);
                        // Every mode now naming the chosen theme shows it; the
                        // other keeps its own.
                        if let Ok(names) = self.config.theme_names() {
                            for mode in [false, true] {
                                if names.pick(mode) == name.trim() {
                                    let side = if mode {
                                        &mut self.themes.dark
                                    } else {
                                        &mut self.themes.light
                                    };
                                    *side = self.theme.clone();
                                }
                            }
                        }
                        // Choosing the other mode's theme leaves this one's on screen.
                        self.theme = self.themes.pick(self.dark).clone();
                    }
                    crate::log_window::set_appearance(&self.config, &self.theme, cx);
                    picker.baseline = None;
                    self.dismiss_menu(window, cx);
                } else {
                    crate::log_window::set_appearance(&self.config, &self.theme, cx);
                    self.drive_picker_load(cx);
                }
            }
            Err(error) => {
                picker.error = Some(error.to_string());
                picker.accepting = false;
            }
        }
        cx.notify();
    }

    pub(super) fn theme_picker_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &mut self.menu.themes else {
            return;
        };
        // Unhandled text must reach the native input system, including IME commands.
        if picker.search.read(cx).is_composing() {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.stop_propagation();
                window.prevent_default();
                self.dismiss_menu(window, cx);
            }
            "up" | "down" if !picker.filtered.is_empty() => {
                cx.stop_propagation();
                window.prevent_default();
                if picker.accepting {
                    return;
                }
                let count = picker.filtered.len();
                picker.selected = (picker.selected
                    + if event.keystroke.key == "up" {
                        count - 1
                    } else {
                        1
                    })
                    % count;
                picker
                    .scroll
                    .scroll_to_item(picker.selected, ScrollStrategy::Center);
                self.preview_picker_selection(cx);
            }
            "enter" => {
                cx.stop_propagation();
                window.prevent_default();
                if let Some(name) = picker.filtered.get(picker.selected).cloned() {
                    self.apply_picker_theme(&name, cx);
                }
            }
            _ => {}
        }
    }

    pub(super) fn render_theme_picker(&self, cx: &mut Context<Self>) -> Div {
        let Some(picker) = &self.menu.themes else {
            return div();
        };
        let theme = &self.theme;
        let font = &self.config.ui;
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .flex_none()
                    .p(px(16.))
                    .border_b_1()
                    .border_color(rgb(theme.active))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(font.size * 1.35))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Color Scheme"),
                            )
                            .child(
                                div()
                                    .id("theme-close")
                                    .debug_selector(|| "theme-close".into())
                                    .px_2()
                                    .py_1()
                                    .cursor_pointer()
                                    .rounded(px(crate::config::corners::CONTROL))
                                    .hover(|s| s.bg(rgb(theme.active)))
                                    .child("Close")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dismiss_menu(window, cx)
                                    })),
                            ),
                    )
                    .child(div().pt(px(12.)).child(picker.search.clone()))
                    .child(div().pt(px(8.)).text_color(rgb(theme.muted)).child(format!(
                        "{} of {} themes",
                        picker.filtered.len(),
                        picker.names.len()
                    ))),
            )
            .when(picker.filtered.is_empty(), |panel| {
                panel.child(
                    div()
                        .debug_selector(|| "theme-empty".into())
                        .flex_1()
                        .p(px(16.))
                        .text_color(rgb(theme.muted))
                        .child("No matching themes. Try a shorter search."),
                )
            })
            .when(!picker.filtered.is_empty(), |panel| {
                panel.child(
                    uniform_list(
                        "theme-results",
                        picker.filtered.len(),
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            let Some(picker) = &this.menu.themes else {
                                return Vec::new();
                            };
                            let current_name = this.current_theme_name();
                            range
                                .map(|index| {
                                    let name = picker.filtered[index].clone();
                                    let selected = index == picker.selected;
                                    let current = name == current_name;
                                    div()
                                        .id(index)
                                        .debug_selector(move || format!("theme-row-{index}"))
                                        // As in the palette, the fill is the row.
                                        .w_full()
                                        .h(px(this.config.ui.line_height() + 20.))
                                        .px(px(16.))
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .cursor_pointer()
                                        .when(selected, |row| row.bg(rgb(this.theme.active)))
                                        .hover(|s| s.bg(rgb(this.theme.active)))
                                        .child(
                                            div()
                                                .debug_selector(|| format!("theme-name-{name}"))
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(name.clone()),
                                        )
                                        .when(current, |row| {
                                            row.child(
                                                div()
                                                    .text_color(rgb(this.theme.muted))
                                                    .child("Current"),
                                            )
                                        })
                                        .on_hover(cx.listener(move |this, hovered, _, cx| {
                                            if *hovered {
                                                if let Some(picker) = &mut this.menu.themes {
                                                    if picker.accepting {
                                                        return;
                                                    }
                                                    picker.selected = index;
                                                }
                                                this.preview_picker_selection(cx);
                                            }
                                        }))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.apply_picker_theme(&name, cx)
                                        }))
                                })
                                .collect()
                        }),
                    )
                    .track_scroll(&picker.scroll)
                    .flex_1()
                    .min_h_0(),
                )
            })
            .child(
                div()
                    .id("theme-status")
                    .debug_selector(|| "theme-status".into())
                    .flex_none()
                    .h(px(font.line_height() * 3. + 20.))
                    .overflow_y_scroll()
                    .px(px(16.))
                    .py(px(10.))
                    .border_t_1()
                    .border_color(rgb(theme.active))
                    .text_color(rgb(theme.muted))
                    .child(if picker.saving {
                        "Saving theme... Please wait.".to_owned()
                    } else if let Some(error) = &picker.error {
                        error.clone()
                    } else if picker.accepting {
                        "Loading theme... Esc to cancel.".to_owned()
                    } else {
                        "Hover or Up / Down to preview. Enter or click a theme to save.".to_owned()
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::config::Themes;
    use core::prelude::v1::test;

    #[gpui::test]
    fn saving_blocks_cancel_replacement_and_reload_until_reconciled(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.simulate_resize(size(px(800.), px(600.)));
        for success in [true, false] {
            let token = cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.config.theme = "Default".into();
                    view.theme = Theme::default();
                    view.open_theme_picker(window, cx);
                    let picker = view.menu.themes.as_mut().unwrap();
                    picker.filtered = vec!["Nord".into()];
                    picker.selected = 0;
                    view.preview_picker_selection(cx);
                    let picker = view.menu.themes.as_mut().unwrap();
                    let token = (picker.session, picker.request);
                    // Hold completion at the disk-write boundary without touching personal config.
                    picker.accepting = true;
                    picker.saving = true;
                    picker.in_flight = Some(token);
                    token
                })
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes("escape");
            let close = cx.debug_bounds("theme-close").unwrap();
            cx.simulate_click(close.center(), Modifiers::default());
            let avatar = cx.debug_bounds("titlebar-avatar").unwrap();
            cx.simulate_click(avatar.center(), Modifiers::default());
            cx.simulate_mouse_down(
                point(px(5.), px(5.)),
                MouseButton::Left,
                Modifiers::default(),
            );
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    assert!(!view.open_menu(window, cx));
                    view.open_preferences(window, cx);
                    view.open_keybinds(window, cx);
                    view.open_theme_picker(window, cx);
                    view.open_palette(false, window, cx);
                    view.show_install_modal(window, cx);
                    view.open_app_update(false, window, cx);
                    view.open_app_update(true, window, cx);
                    assert!(view.update_preview.is_none());
                    let snapshot: herdr_client::protocol::ClientShellSnapshot =
                        serde_json::from_str(include_str!(
                            "../../herdr-protocol/tests/fixtures/endpoint-snapshot-v1.json"
                        ))
                        .unwrap();
                    let tab = snapshot.tabs[0].tab_id.clone();
                    view.live.snapshot = Some(std::sync::Arc::new(snapshot));
                    view.open_tab_menu(&tab, Point::default(), window, cx);
                    view.open_tab_close(&tab, window, cx);
                    view.open_close_confirmation(crate::controls::Command::CloseTab, window, cx);
                    view.reload_gui_config(window, cx);
                    assert!(view.menu.page == Some(Page::Themes));
                    assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
                    assert_eq!(view.config.theme, "Default");
                    let picker = view.menu.themes.as_ref().unwrap();
                    assert_eq!((picker.session, picker.request), token);
                    assert!(picker.saving);
                    let result = if success {
                        Ok(Theme::builtin("Nord").unwrap())
                    } else {
                        Err(crate::Error::MissingHome)
                    };
                    view.finish_picker_load(token, true, result, window, cx);
                    assert!(!view.theme_save_in_flight());
                    if success {
                        assert_eq!(view.config.theme, "Nord");
                        assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
                        assert!(view.menu.page.is_none());
                    } else {
                        assert!(view.menu.page == Some(Page::Themes));
                        assert!(view.menu.themes.as_ref().unwrap().error.is_some());
                        view.dismiss_menu(window, cx);
                        assert_eq!(view.theme, Theme::default());
                        assert_eq!(view.config.theme, "Default");
                    }
                })
            });
        }
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_theme_picker(window, cx);
                let picker = view.menu.themes.as_mut().unwrap();
                let token = (picker.session, picker.request);
                picker.in_flight = Some(token);
                picker.filtered = vec!["/pending-theme".into()];
                picker.selected = 0;
                view.apply_picker_theme("/pending-theme", cx);
                assert!(!view.theme_save_in_flight());
                view.dismiss_menu(window, cx);
                view.finish_picker_load(
                    token,
                    false,
                    Ok(Theme::builtin("Nord").unwrap()),
                    window,
                    cx,
                );
                assert!(view.menu.page.is_none());
                assert_eq!(view.theme, Theme::default());
                assert_eq!(view.config.theme, "Default");
                assert!(view.menu.themes.as_ref().unwrap().in_flight.is_none());
            })
        });
    }

    #[gpui::test]
    fn status_changes_do_not_move_rows_under_pointer(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| view.update(cx, |view, cx| view.open_theme_picker(window, cx)));
        cx.run_until_parked();
        for width in [800., 360.] {
            cx.simulate_resize(size(px(width), px(600.)));
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let row = cx.debug_bounds("theme-row-1").unwrap();
            let status = cx.debug_bounds("theme-status").unwrap();
            // Selection paints as a row: the fill spans the list, not the label.
            assert_eq!(row.size.width, status.size.width);
            assert_eq!(row.left(), status.left());
            cx.simulate_mouse_move(row.center(), None, Modifiers::default());
            for (error, accepting, saving) in [
                (
                    Some("Very long theme load failure: ".repeat(40)),
                    false,
                    false,
                ),
                (None, true, false),
                (None, true, true),
                (None, false, false),
            ] {
                cx.update(|window, cx| {
                    view.update(cx, |view, cx| {
                        let picker = view.menu.themes.as_mut().unwrap();
                        picker.error = error;
                        picker.accepting = accepting;
                        picker.saving = saving;
                        cx.notify();
                    });
                    window.draw(cx).clear(cx);
                });
                assert_eq!(cx.debug_bounds("theme-row-1").unwrap(), row);
                assert_eq!(cx.debug_bounds("theme-status").unwrap(), status);
                view.read_with(cx, |view, _| {
                    assert_eq!(view.menu.themes.as_ref().unwrap().selected, 1)
                });
            }
        }
    }

    #[gpui::test]
    fn searched_picker_reopens_on_nonfirst_configured_theme(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.config.theme = "Nord".into();
                view.theme = Theme::builtin("Nord").unwrap();
                view.open_theme_picker(window, cx);
            })
        });
        cx.run_until_parked();
        cx.simulate_input("Dracula");
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| view.update(cx, |view, cx| view.open_theme_picker(window, cx)));
        // Drain the programmatic clear's Changed event and discovery completion.
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let picker = view.menu.themes.as_ref().unwrap();
            assert!(picker.search.read(cx).text().is_empty());
            assert!(picker.query.is_empty());
            assert!(picker.selected > 0);
            assert_eq!(picker.filtered[picker.selected], "Nord");
            assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
            assert_eq!(view.config.theme, "Nord");
            assert!(picker.desired.is_none());
        });
    }

    #[gpui::test]
    fn discovery_is_single_flight_across_reopens_and_ignores_old_errors(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| view.update(cx, |view, cx| view.open_theme_picker(window, cx)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let picker = view.menu.themes.as_mut().unwrap();
                let session = picker.session;
                picker.discovering = true;
                for _ in 0..30 {
                    view.dismiss_menu(window, cx);
                    view.open_theme_picker(window, cx);
                    assert!(view.menu.themes.as_ref().unwrap().discovering);
                }
                view.finish_picker_discovery(
                    session,
                    Ok(vec!["Default".into(), "scan-result".into()]),
                    cx,
                );
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let picker = view.menu.themes.as_mut().unwrap();
                assert!(!picker.discovering);
                assert_eq!(picker.names, ["Default", "scan-result"]);
                let session = picker.session;
                picker.discovering = true;
                view.dismiss_menu(window, cx);
                view.open_theme_picker(window, cx);
                view.finish_picker_discovery(session, Err(crate::Error::MissingHome), cx);
                let picker = view.menu.themes.as_ref().unwrap();
                assert!(!picker.discovering);
                assert!(picker.error.is_none());
            })
        });
    }

    #[gpui::test]
    fn hover_keys_search_preview_and_dismiss_restore_original(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.simulate_resize(size(px(800.), px(600.)));
        cx.update(|window, cx| view.update(cx, |view, cx| view.open_theme_picker(window, cx)));
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, _| {
                let picker = view.menu.themes.as_mut().unwrap();
                picker.names = vec!["Default".into(), "Nord".into(), "Dracula".into()];
                picker.filter("");
            });
            window.draw(cx).clear(cx);
        });
        let row = cx.debug_bounds("theme-row-1").unwrap();
        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        view.read_with(cx, |view, _| {
            assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
            assert_eq!(view.config.theme, "Default");
            assert_eq!(view.menu.themes.as_ref().unwrap().selected, 1);
        });
        cx.simulate_keystrokes("down");
        view.read_with(cx, |view, _| {
            assert_eq!(view.theme, Theme::builtin("Dracula").unwrap())
        });
        cx.simulate_keystrokes("up");
        view.read_with(cx, |view, _| {
            assert_eq!(view.theme, Theme::builtin("Nord").unwrap())
        });
        cx.simulate_input("Dracula");
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.theme, Theme::builtin("Dracula").unwrap());
            assert_eq!(view.config.theme, "Default");
            assert!(!view.menu.themes.as_ref().unwrap().accepting);
            assert!(view.menu.themes.as_ref().unwrap().in_flight.is_none());
        });
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| assert_eq!(view.theme, Theme::default()));

        for dismiss in 0..3 {
            cx.update(|window, cx| {
                view.update(cx, |view, cx| {
                    view.open_theme_picker(window, cx);
                    let picker = view.menu.themes.as_mut().unwrap();
                    picker.filtered = vec!["Nord".into()];
                    picker.selected = 0;
                    view.preview_picker_selection(cx);
                    assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
                    if dismiss == 0 {
                        view.open_preferences(window, cx);
                    }
                    if dismiss == 1 {
                        view.dismiss_menu(window, cx);
                    }
                })
            });
            if dismiss == 2 {
                cx.update(|window, cx| window.draw(cx).clear(cx));
                cx.simulate_mouse_down(
                    point(px(5.), px(5.)),
                    MouseButton::Left,
                    Modifiers::default(),
                );
            }
            view.read_with(cx, |view, _| assert_eq!(view.theme, Theme::default()));
        }
    }

    #[gpui::test]
    fn load_completion_coalesces_and_fences_requests_and_sessions(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_theme_picker(window, cx);
                // Hold the single load slot explicitly. No filesystem or timing guesses.
                let picker = view.menu.themes.as_mut().unwrap();
                let old = (picker.session, picker.request);
                picker.in_flight = Some(old);
                picker.filtered = vec!["file-a".into(), "file-b".into(), "Nord".into()];
                picker.selected = 0;
                view.preview_picker_selection(cx);
                view.menu.themes.as_mut().unwrap().selected = 1;
                view.preview_picker_selection(cx);
                assert_eq!(view.menu.themes.as_ref().unwrap().in_flight, Some(old));
                assert_eq!(
                    view.menu.themes.as_ref().unwrap().desired.as_deref(),
                    Some("file-b")
                );
                view.menu.themes.as_mut().unwrap().selected = 2;
                view.preview_picker_selection(cx);
                view.finish_picker_load(old, false, Err(crate::Error::MissingHome), window, cx);
                assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
                assert!(view.menu.themes.as_ref().unwrap().error.is_none());
                assert!(view.menu.themes.as_ref().unwrap().in_flight.is_none());

                let picker = view.menu.themes.as_mut().unwrap();
                let old = (picker.session, picker.request);
                picker.in_flight = Some(old);
                view.dismiss_menu(window, cx);
                view.open_theme_picker(window, cx);
                view.finish_picker_load(
                    old,
                    false,
                    Ok(Theme::builtin("Dracula").unwrap()),
                    window,
                    cx,
                );
                assert_eq!(view.theme, Theme::default());
                assert!(view.menu.themes.as_ref().unwrap().error.is_none());
                assert!(view.menu.themes.as_ref().unwrap().in_flight.is_none());
                // A duplicate/foreign completion cannot release another request's slot.
                view.menu.themes.as_mut().unwrap().in_flight = Some((999, 999));
                view.finish_picker_load(old, false, Err(crate::Error::MissingHome), window, cx);
                assert_eq!(
                    view.menu.themes.as_ref().unwrap().in_flight,
                    Some((999, 999))
                );
            })
        });
    }

    #[gpui::test]
    fn background_load_runs_latest_target_and_failure_preserves_preview(cx: &mut TestAppContext) {
        let path =
            std::env::temp_dir().join(format!("herdr-picker-preview-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        file.write_all(b"background=123456").unwrap();
        drop(file);
        let name = path.to_str().unwrap().to_owned();
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_theme_picker(window, cx);
                let picker = view.menu.themes.as_mut().unwrap();
                let old = (picker.session, picker.request);
                picker.in_flight = Some(old);
                picker.filtered = vec!["/obsolete-theme".into(), name.clone()];
                picker.selected = 0;
                view.preview_picker_selection(cx);
                view.menu.themes.as_mut().unwrap().selected = 1;
                view.preview_picker_selection(cx);
                // Completing an obsolete request starts exactly the latest file load.
                view.finish_picker_load(old, false, Err(crate::Error::MissingHome), window, cx);
                let picker = view.menu.themes.as_ref().unwrap();
                assert_eq!(picker.in_flight, Some((picker.session, picker.request)));
                assert_eq!(view.theme, Theme::default());
            })
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.theme.background, 0x123456);
            assert_eq!(view.config.theme, "Default");
            assert!(view.menu.themes.as_ref().unwrap().in_flight.is_none());
        });
        std::fs::remove_file(&path).unwrap();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let picker = view.menu.themes.as_mut().unwrap();
                picker.filtered = vec![name];
                picker.selected = 0;
                picker.desired = None;
                view.preview_picker_selection(cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert_eq!(view.theme.background, 0x123456);
                assert!(view.menu.themes.as_ref().unwrap().error.is_some());
                assert_eq!(view.config.theme, "Default");
                view.dismiss_menu(window, cx);
                assert_eq!(view.theme, Theme::default());
            })
        });
    }

    #[gpui::test]
    fn accept_completion_retains_preview_and_failure_remains_cancellable(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_theme_picker(window, cx);
                let picker = view.menu.themes.as_mut().unwrap();
                picker.filtered = vec!["Nord".into()];
                picker.selected = 0;
                view.preview_picker_selection(cx);
                let picker = view.menu.themes.as_mut().unwrap();
                let token = (picker.session, picker.request);
                picker.in_flight = Some(token);
                view.theme_picker_key(
                    &KeyDownEvent {
                        keystroke: Keystroke::parse("enter").unwrap(),
                        is_held: false,
                        prefer_character_input: false,
                    },
                    window,
                    cx,
                );
                assert!(view.menu.themes.as_ref().unwrap().accepting);
                assert_eq!(view.config.theme, "Default");
                view.finish_picker_load(token, true, Err(crate::Error::MissingHome), window, cx);
                assert!(view.menu.themes.as_ref().unwrap().error.is_some());
                assert_eq!(view.config.theme, "Default");
                view.dismiss_menu(window, cx);
                assert_eq!(view.theme, Theme::default());

                view.open_theme_picker(window, cx);
                let picker = view.menu.themes.as_mut().unwrap();
                picker.filtered = vec!["Nord".into()];
                picker.selected = 0;
                view.preview_picker_selection(cx);
                let picker = view.menu.themes.as_mut().unwrap();
                let token = (picker.session, picker.request);
                picker.in_flight = Some(token);
                view.apply_picker_theme("Nord", cx);
                view.finish_picker_load(
                    token,
                    true,
                    Ok(Theme::builtin("Nord").unwrap()),
                    window,
                    cx,
                );
                assert!(view.menu.page.is_none());
                assert_eq!(view.config.theme, "Nord");
                assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
                view.dismiss_menu(window, cx);
                assert_eq!(view.theme, Theme::builtin("Nord").unwrap());
            })
        });
    }

    /// With a light/dark pair, the picker shows and replaces the theme of the
    /// mode in effect and keeps the other; a mode switch during a preview
    /// changes what cancelling returns to.
    #[gpui::test]
    fn picking_with_a_pair_replaces_the_current_modes_half(cx: &mut TestAppContext) {
        let latte = Theme::builtin("Catppuccin Latte").unwrap();
        let nord = Theme::builtin("Nord").unwrap();
        let dracula = Theme::builtin("Dracula").unwrap();
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.config.theme = "light:Catppuccin Latte,dark:Nord".into();
                view.themes = Themes {
                    light: latte.clone(),
                    dark: nord.clone(),
                };
                view.dark = true;
                view.theme = nord.clone();
                view.open_theme_picker(window, cx);
                assert_eq!(view.current_theme_name(), "Nord");
                let picker = view.menu.themes.as_ref().unwrap();
                assert_eq!(picker.filtered[picker.selected], "Nord");

                // Switching to light mid-preview keeps the preview up, and
                // cancelling it shows the light theme.
                let picker = view.menu.themes.as_mut().unwrap();
                picker.filtered = vec!["Dracula".into()];
                picker.selected = 0;
                view.preview_picker_selection(cx);
                assert_eq!(view.theme, dracula);
                view.config.appearance = crate::config::Appearance::Light;
                view.sync_appearance(window, cx);
                assert_eq!(view.theme, dracula);
                view.dismiss_menu(window, cx);
                assert_eq!(view.theme, latte);

                // Back in dark mode, accepting Dracula replaces only its half.
                view.config.appearance = crate::config::Appearance::Dark;
                view.sync_appearance(window, cx);
                assert_eq!(view.theme, nord);
                view.open_theme_picker(window, cx);
                let picker = view.menu.themes.as_mut().unwrap();
                picker.filtered = vec!["Dracula".into()];
                picker.selected = 0;
                view.preview_picker_selection(cx);
                let picker = view.menu.themes.as_mut().unwrap();
                let token = (picker.session, picker.request);
                picker.in_flight = Some(token);
                view.apply_picker_theme("Dracula", cx);
                view.finish_picker_load(token, true, Ok(dracula.clone()), window, cx);
                assert_eq!(view.config.theme, "light:Catppuccin Latte,dark:Dracula");
                assert_eq!(view.themes.light, latte);
                assert_eq!(view.themes.dark, dracula);
                assert_eq!(view.theme, dracula);
            })
        });
    }

    /// Choosing a theme for one mode saves that mode's own setting, leaving
    /// `theme` and the other mode alone, and the screen on the current mode's
    /// theme. The Themes command then keeps using a mode's own setting.
    #[gpui::test]
    fn picking_for_one_mode_sets_its_own_theme(cx: &mut TestAppContext) {
        let latte = Theme::builtin("Catppuccin Latte").unwrap();
        let nord = Theme::builtin("Nord").unwrap();
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        let accept = |view: &mut HerdrWindow,
                      target: ThemeTarget,
                      name: &str,
                      theme: &Theme,
                      window: &mut Window,
                      cx: &mut Context<HerdrWindow>| {
            view.open_theme_picker_for(target, window, cx);
            let picker = view.menu.themes.as_mut().unwrap();
            picker.filtered = vec![name.into()];
            picker.selected = 0;
            view.preview_picker_selection(cx);
            let picker = view.menu.themes.as_mut().unwrap();
            let token = (picker.session, picker.request);
            picker.in_flight = Some(token);
            view.apply_picker_theme(name, cx);
            view.finish_picker_load(token, true, Ok(theme.clone()), window, cx);
        };
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.config.theme = "Nord".into();
                view.themes = Themes::single(nord.clone());
                view.dark = true;
                view.theme = nord.clone();
                view.open_theme_picker_for(ThemeTarget::Mode { dark: false }, window, cx);
                // The light row's picker marks the light mode's theme.
                assert_eq!(view.current_theme_name(), "Nord");
                view.dismiss_menu(window, cx);

                let light = ThemeTarget::Mode { dark: false };
                accept(view, light, "Catppuccin Latte", &latte, window, cx);
                assert_eq!(view.config.theme, "Nord");
                assert_eq!(view.config.light_theme.as_deref(), Some("Catppuccin Latte"));
                assert_eq!(view.config.dark_theme, None);
                assert_eq!(view.themes.light, latte);
                assert_eq!(view.themes.dark, nord);
                // Dark mode is on screen, so its theme stays.
                assert_eq!(view.theme, nord);
                assert_eq!(view.theme_name_for(false), "Catppuccin Latte");
                assert_eq!(view.theme_name_for(true), "Nord");

                // The Themes command in dark mode changes `theme`, which
                // light mode no longer follows.
                let dracula = Theme::builtin("Dracula").unwrap();
                accept(view, ThemeTarget::Current, "Dracula", &dracula, window, cx);
                assert_eq!(view.config.theme, "Dracula");
                assert_eq!(view.config.light_theme.as_deref(), Some("Catppuccin Latte"));
                assert_eq!(view.themes.light, latte);
                assert_eq!(view.themes.dark, dracula);
                assert_eq!(view.theme, dracula);

                // In light mode, the command keeps light mode's own setting.
                view.dark = false;
                view.theme = latte.clone();
                accept(view, ThemeTarget::Current, "Nord", &nord, window, cx);
                assert_eq!(view.config.light_theme.as_deref(), Some("Nord"));
                assert_eq!(view.config.theme, "Dracula");
                assert_eq!(view.themes.light, nord);
                assert_eq!(view.themes.dark, dracula);
                assert_eq!(view.theme, nord);
            })
        });
    }
}
