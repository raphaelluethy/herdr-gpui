#![allow(clippy::unwrap_used)]

use super::{Location, Store, WebUrl, view::scope};
#[cfg(unix)]
use crate::control::{Placed, Target};
use crate::{
    HerdrWindow,
    sidebar::layout_tests::{fixture_window, full_draw, snapshot},
};
use gpui::{Entity, VisualTestContext};
use std::sync::Arc;

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| full_draw(window, cx).clear(cx));
}

fn url(value: &str) -> Location {
    Location::Web {
        url: WebUrl::try_from(value).unwrap(),
    }
}

/// The fixture window, showing workspace `w0` as a connected daemon would.
fn window(cx: &mut gpui::TestAppContext) -> (Entity<HerdrWindow>, &mut VisualTestContext) {
    cx.add_window_view(|window, cx| {
        let mut view = fixture_window(window, cx);
        let mut shown = snapshot(40);
        shown.focused_workspace_id = Some("w0".into());
        shown.focused_tab_id = Some("t0".into());
        view.live.snapshot = Some(Arc::new(shown));
        view
    })
}

/// Needs a build that shows pages: elsewhere a new tab opens nothing.
#[cfg(any(target_os = "macos", windows))]
mod embedded {
    use super::*;
    use crate::controls::Command;

    /// How many browser tabs the app holds; test IDs start at zero.
    fn tab_count(cx: &mut VisualTestContext) -> usize {
        cx.update(|_, cx| {
            cx.try_global::<Store>().map_or(0, |store| {
                (0..64)
                    .filter(|id| store.get(crate::browser::TabId::test(*id)).is_some())
                    .count()
            })
        })
    }

    #[gpui::test]
    fn a_browser_tab_covers_the_terminal_until_it_closes(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        draw(cx);
        assert!(cx.debug_bounds("terminal").is_some());
        assert!(cx.debug_bounds("browser").is_none());

        // A blank tab needs no page, so this runs without a native web view.
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.command(Command::NewBrowserTab, window, cx)
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("browser-tab-0").is_some());
        assert!(cx.debug_bounds("browser").is_some());
        assert!(cx.debug_bounds("browser-address").is_some());
        assert!(cx.debug_bounds("browser-placeholder").is_some());
        assert!(
            cx.debug_bounds("terminal").is_none(),
            "the page replaces it"
        );

        // Clicking a Herdr tab brings its terminal back; the browser tab stays.
        let herdr_tab = cx.debug_bounds("tab-t0").unwrap();
        cx.simulate_click(herdr_tab.center(), gpui::Modifiers::none());
        draw(cx);
        assert!(cx.debug_bounds("terminal").is_some());
        assert!(cx.debug_bounds("browser-tab-0").is_some());

        // Close Tab closes the page rather than asking about the Herdr tab.
        let browser_tab = cx.debug_bounds("browser-tab-0").unwrap();
        cx.simulate_click(browser_tab.center(), gpui::Modifiers::none());
        draw(cx);
        assert!(cx.debug_bounds("browser").is_some());
        assert!(cx.debug_bounds("annotations").is_none());

        // Annotating opens the notes panel beside the page.
        let before = std::time::Instant::now();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.toggle_annotating(crate::browser::TabId::test(0), window, cx)
            });
        });
        draw(cx);
        assert!(cx.debug_bounds("annotations").is_some());
        // It slides open rather than appearing whole. Asked about a moment
        // before it opened, which reads as not yet begun, so a draw slower
        // than the slide itself cannot finish it first.
        view.read_with(cx, |view, _| {
            assert!(view.browser.annotations.moving(before));
        });
        assert!(cx.debug_bounds("browser-annotate").is_some());

        cx.update(|window, cx| {
            view.update(cx, |view, cx| view.command(Command::CloseTab, window, cx));
        });
        draw(cx);
        assert!(cx.debug_bounds("browser-tab-0").is_none());
        assert!(cx.debug_bounds("terminal").is_some());
        view.read_with(cx, |view, _| assert!(view.menu.page.is_none()));
        assert_eq!(tab_count(cx), 0);
    }
}

/// Editor groups. Blank browser tabs need no native page, so none is
/// created, and a split alone needs no page at all.
mod groups {
    use super::*;
    use crate::{
        browser::{GroupId, Pick, Shown},
        controls::Command,
    };

    fn groups(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext) -> Vec<GroupId> {
        draw(cx);
        view.read_with(cx, |view, _| {
            view.group_slots().into_iter().map(|slot| slot.id).collect()
        })
    }

    fn shown(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext) -> Vec<Shown> {
        let groups = groups(view, cx);
        cx.update(|_, cx| {
            groups
                .iter()
                .map(|group| view.read(cx).group_shown(*group, cx))
                .collect()
        })
    }

    fn run(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext, command: Command) {
        cx.update(|window, cx| view.update(cx, |view, cx| view.command(command, window, cx)));
        draw(cx);
    }

    /// Debug selectors are looked up as `'static`; tests name a few.
    fn selector(name: String) -> &'static str {
        Box::leak(name.into_boxed_str())
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        draw(cx);
    }

    fn herdr(tab: &str) -> Pick {
        Pick::Herdr(tab.into())
    }

    #[gpui::test]
    fn splitting_only_splits(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        draw(cx);
        assert_eq!(shown(&view, cx), [Shown::Terminal]);
        click(cx, "split-editor");
        // The new group takes the tab; nothing new opens.
        assert_eq!(
            shown(&view, cx),
            [Shown::Elsewhere(herdr("t0")), Shown::Terminal]
        );
        assert!(
            cx.update(|_, cx| view.read(cx).browser_tab_ids(cx))
                .is_empty()
        );
        assert!(cx.debug_bounds("stand-in").is_some());
        assert!(cx.debug_bounds("g1-tab-t0").is_some());
        // Split as often as wanted, from any group.
        click(cx, "g1-split-editor");
        run(&view, cx, Command::SplitEditor);
        assert_eq!(groups(&view, cx).len(), 4);
        for divider in 0..3 {
            let selector = selector(format!("group-divider-{divider}"));
            assert!(cx.debug_bounds(selector).is_some(), "{selector}");
        }
        // Showing it here brings the terminal back to the first group.
        click(cx, "show-here");
        let terminal = cx.debug_bounds("terminal").unwrap();
        let first = cx.debug_bounds("group").unwrap();
        assert!(terminal.left() < first.right());
        assert_eq!(shown(&view, cx)[0], Shown::Terminal);
    }

    /// Needs a build that shows pages: elsewhere a new tab opens nothing.
    #[cfg(any(target_os = "macos", windows))]
    #[gpui::test]
    fn a_page_and_the_terminal_sit_side_by_side(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        draw(cx);
        run(&view, cx, Command::SplitEditor);
        let [left, right] = groups(&view, cx)[..] else {
            panic!("two groups")
        };
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_browser_tab_in(right, window, cx);
                view.activate_group(left, window, cx);
            })
        });
        let page = crate::browser::TabId::test(0);
        assert_eq!(shown(&view, cx), [Shown::Terminal, Shown::Page(page)]);
        assert!(cx.debug_bounds("terminal").is_some());
        assert!(cx.debug_bounds("g1-browser").is_some());
        // Every group lists every tab.
        for selector in ["tab-t0", "browser-tab-0", "g1-tab-t0", "g1-browser-tab-0"] {
            assert!(cx.debug_bounds(selector).is_some(), "{selector}");
        }
        // Picking the page on the left as well moves it there.
        click(cx, "browser-tab-0");
        assert_eq!(
            shown(&view, cx),
            [Shown::Page(page), Shown::Elsewhere(Pick::Page(page))]
        );
        // Closing the page leaves both groups following the terminal again.
        run(&view, cx, Command::CloseTab);
        assert!(
            cx.update(|_, cx| view.read(cx).browser_tab_ids(cx))
                .is_empty()
        );
        assert_eq!(
            shown(&view, cx),
            [Shown::Terminal, Shown::Elsewhere(herdr("t0"))]
        );
    }

    #[gpui::test]
    fn picking_another_herdr_tab_focuses_it_in_the_daemon(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        draw(cx);
        run(&view, cx, Command::SplitEditor);
        let other = view.read_with(cx, |view, _| {
            let snapshot = view.live.snapshot.as_ref().unwrap();
            snapshot
                .tabs
                .iter()
                .find(|tab| tab.workspace_id == "w0" && tab.tab_id != "t0")
                .map(|tab| tab.tab_id.clone())
        });
        let Some(other) = other else {
            return;
        };
        click(cx, selector(format!("g1-tab-{other}")));
        // Until the daemon focuses it, the group stands in for it.
        assert_eq!(
            shown(&view, cx)[1],
            Shown::Elsewhere(Pick::Herdr(other.clone()))
        );
        // Once it does, the group shows it and the other keeps its tab.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let snapshot = Arc::make_mut(view.live.snapshot.as_mut().unwrap());
                snapshot.focused_tab_id = Some(other.clone());
                view.terminal_focus_moved(&other, false, cx);
            })
        });
        assert_eq!(
            shown(&view, cx),
            [Shown::Elsewhere(herdr("t0")), Shown::Terminal]
        );
    }

    #[gpui::test]
    fn a_saved_layout_comes_back_and_changes_are_recorded(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        let saved = r#"{"groups":[{"pick":{"kind":"herdr","id":"t0"},"share":0.3},{"pick":{"kind":"herdr","id":"t1"},"share":0.7}],"active":0}"#;
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                // As on a first show of the workspace since the restart.
                let key = view.browser_key().unwrap();
                view.browser.layouts.remove(&key);
                view.browser
                    .saved
                    .insert(key, serde_json::from_str(saved).unwrap());
            })
        });
        let shown = shown(&view, cx);
        assert_eq!(shown.len(), 2);
        // The group in use shows its tab; the other waits for its own
        // connection, which the fixture never gets.
        assert_eq!(shown[0], Shown::Terminal);
        assert_eq!(shown[1], Shown::Elsewhere(herdr("t1")));
        let (key, share) = view.read_with(cx, |view, _| {
            let slots = view.group_slots();
            (view.browser_key().unwrap(), view.group_share(slots[0].id))
        });
        assert!((share - 0.3).abs() < 1e-6);
        // The restored layout is saved as it is, then as it changes.
        cx.update(|_, cx| view.update(cx, |view, cx| view.save_group_layouts(cx)));
        let recorded = cx.update(|_, cx| crate::browser::Layouts::snapshot(cx));
        assert_eq!(
            serde_json::to_string(recorded.get(&key).unwrap()).unwrap(),
            saved
        );
        run(&view, cx, Command::SplitEditor);
        cx.update(|_, cx| view.update(cx, |view, cx| view.save_group_layouts(cx)));
        let recorded = cx.update(|_, cx| crate::browser::Layouts::snapshot(cx));
        let json = serde_json::to_string(recorded.get(&key).unwrap()).unwrap();
        assert_eq!(json.matches("\"share\"").count(), 3, "{json}");
        // Closing back to one group following the terminal forgets it.
        for group in groups(&view, cx).into_iter().skip(1) {
            cx.update(|window, cx| view.update(cx, |view, cx| view.close_group(group, window, cx)));
        }
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let group = view.group_slots()[0].id;
                let layout = view.ensure_layout().unwrap();
                layout.replace(&herdr("t0"), None, None);
                let _ = group;
                view.save_group_layouts(cx);
            })
        });
        let recorded = cx.update(|_, cx| crate::browser::Layouts::snapshot(cx));
        assert!(!recorded.contains_key(&key));
    }

    #[gpui::test]
    fn a_split_opens_from_the_right_and_a_close_folds_away(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        cx.update(|_, cx| view.update(cx, |view, _| view.browser.group_motion.enable()));
        draw(cx);
        let whole = cx.debug_bounds("group").unwrap();
        run(&view, cx, Command::SplitEditor);
        let early = crate::motion::ENTER / 8;
        cx.update(|_, cx| view.update(cx, |view, _| view.browser.group_motion.freeze(early)));
        draw(cx);
        // Early on, the source still has most of the row, and the new group,
        // laid out at its settled width, is only partly uncovered from the
        // right: its content runs past the source's edge to the left.
        let source = cx.debug_bounds("group").unwrap();
        let opening = cx.debug_bounds("g1-group").unwrap();
        assert!(source.size.width > whole.size.width * 0.55, "{source:?}");
        assert!(opening.size.width > whole.size.width * 0.45, "{opening:?}");
        assert!(opening.left() < source.right(), "{opening:?} {source:?}");
        assert!(
            (opening.right() - whole.right()).abs() < gpui::px(2.),
            "{opening:?} {whole:?}"
        );
        view.read_with(cx, |view, _| {
            let group = view.group_slots()[1].id;
            let opened = view.group_opened(group).unwrap_or(1.);
            assert!(opened > 0. && opened < 1., "{opened}");
        });

        // Settled, then closed: the group folds away where it stood.
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.browser.group_motion = Default::default();
                view.browser.group_motion.enable();
            })
        });
        let [_, right] = groups(&view, cx)[..] else {
            panic!("two groups")
        };
        cx.update(|window, cx| view.update(cx, |view, cx| view.close_group(right, window, cx)));
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.browser.group_motion.freeze(std::time::Duration::ZERO)
            })
        });
        draw(cx);
        let folding = cx.debug_bounds("folding-group").unwrap();
        let left = cx.debug_bounds("group").unwrap();
        assert!(
            folding.left() >= left.right() - gpui::px(1.),
            "{folding:?} {left:?}"
        );
        assert!(folding.size.width > whole.size.width / 3., "{folding:?}");
        view.read_with(cx, |view, _| assert_eq!(view.folding_groups().len(), 1));
    }

    #[gpui::test]
    fn empty_groups_and_dividers(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        draw(cx);
        run(&view, cx, Command::SplitEditor);
        let divider = cx.debug_bounds("group-divider-0").unwrap();
        let before = cx.debug_bounds("group").unwrap();
        let target = divider.center() - gpui::point(gpui::px(100.), gpui::px(0.));
        cx.simulate_mouse_down(
            divider.center(),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(
            target,
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_move(
            target,
            Some(gpui::MouseButton::Left),
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(target, gpui::MouseButton::Left, gpui::Modifiers::none());
        draw(cx);
        let after = cx.debug_bounds("group").unwrap();
        assert!(after.size.width < before.size.width - gpui::px(50.));
        // Closing a group hands its tab strip back to one group.
        let [_, right] = groups(&view, cx)[..] else {
            panic!("two groups")
        };
        cx.update(|window, cx| view.update(cx, |view, cx| view.close_group(right, window, cx)));
        assert_eq!(shown(&view, cx), [Shown::Terminal]);
        assert!(cx.debug_bounds("group-divider-0").is_none());
    }
}

/// A browser tab is this client's own, so dropping one reorders the
/// workspace's tabs at once, without a daemon.
#[gpui::test]
fn dragging_a_browser_tab_reorders_the_workspace_s_tabs(cx: &mut gpui::TestAppContext) {
    use gpui::{Modifiers, MouseButton, point, px};

    let (view, cx) = window(cx);
    cx.simulate_resize(gpui::size(px(1600.), px(600.)));
    let ids = cx.update(|_, cx| {
        let scope = scope(&view.read(cx).endpoints[0]);
        (0..3)
            .map(|_| {
                Store::update(cx, |store| {
                    store.open(scope.clone(), "w0", None, None).unwrap()
                })
            })
            .collect::<Vec<_>>()
    });
    cx.update(|_, cx| view.update(cx, |view, _| view.browser.appear = Default::default()));
    draw(cx);
    draw(cx);
    let order = |view: &Entity<HerdrWindow>, cx: &mut VisualTestContext| {
        cx.update(|_, cx| {
            let scope = scope(&view.read(cx).endpoints[0]);
            cx.global::<Store>()
                .in_workspace(&scope, "w0")
                .map(|tab| tab.id)
                .collect::<Vec<_>>()
        })
    };
    let first = cx.debug_bounds("browser-tab-0").unwrap();
    let second = cx.debug_bounds("browser-tab-1").unwrap();
    // Browser tabs move among their own: over the Herdr tab ahead of them,
    // the first stays first.
    let herdr = cx.debug_bounds("tab-t0").unwrap();
    cx.simulate_mouse_down(first.center(), MouseButton::Left, Modifiers::default());
    for _ in 0..2 {
        cx.simulate_mouse_move(herdr.center(), MouseButton::Left, Modifiers::default());
        draw(cx);
    }
    view.read_with(cx, |view, _| {
        assert!(!view.tab_drag.as_ref().unwrap().has_target())
    });
    let over = point(second.right() - px(2.), first.center().y);
    for _ in 0..2 {
        cx.simulate_mouse_move(over, MouseButton::Left, Modifiers::default());
        draw(cx);
    }
    cx.simulate_mouse_up(over, MouseButton::Left, Modifiers::default());
    draw(cx);
    view.read_with(cx, |view, _| assert!(view.tab_drag.is_none()));
    assert_eq!(order(&view, cx), [ids[1], ids[0], ids[2]]);
    // The release was the drop, not a click showing the tab.
    view.read_with(cx, |view, _| {
        assert_ne!(
            view.group_pick(view.group_slots()[0].id),
            Some(super::Pick::Page(ids[0]))
        )
    });
    assert_eq!(
        cx.debug_bounds("browser-tab-1").unwrap().left(),
        first.left()
    );
}

/// Tabs a workspace already has appear at once; one that opens later grows
/// into the strip.
#[gpui::test]
fn a_tab_that_opens_grows_into_the_strip(cx: &mut gpui::TestAppContext) {
    let (view, cx) = window(cx);
    draw(cx);
    let tab = cx.debug_bounds("tab-t0").unwrap();
    assert!(tab.size.width >= gpui::px(crate::TAB_WIDTH));
    view.read_with(cx, |view, _| assert!(!view.tabs_growing()));
    cx.update(|_, cx| {
        let scope = scope(&view.read(cx).endpoints[0]);
        Store::update(cx, |store| store.open(scope, "w0", None, None));
    });
    draw(cx);
    view.read_with(cx, |view, _| assert!(view.tabs_growing()));
    // Held where it started: narrow and clear.
    cx.update(|_, cx| view.update(cx, |view, _| view.browser.appear.hold()));
    draw(cx);
    let growing = cx.debug_bounds("browser-tab-0").unwrap();
    assert!(
        growing.size.width < gpui::px(crate::TAB_WIDTH / 2.),
        "{growing:?}"
    );

    // Closed, it shrinks out where it stood, from its full width.
    cx.update(|_, cx| view.update(cx, |view, _| view.browser.appear = Default::default()));
    draw(cx);
    let whole = cx.debug_bounds("browser-tab-0").unwrap();
    cx.update(|_, cx| Store::update(cx, |store| store.close(crate::browser::TabId::test(0))));
    draw(cx);
    assert!(cx.debug_bounds("browser-tab-0").is_none());
    cx.update(|_, cx| view.update(cx, |view, _| view.browser.appear.hold()));
    draw(cx);
    let leaving = cx.debug_bounds("leaving-tab").unwrap();
    assert_eq!(leaving.left(), whole.left());
    // Its measured width, within a pixel or two of the tab it replaces.
    assert!(
        (leaving.size.width - whole.size.width).abs() < gpui::px(4.),
        "{leaving:?} {whole:?}"
    );
    view.read_with(cx, |view, _| assert!(view.tabs_growing()));
}

/// Opens a request's tab without switching to it, so no native page is made.
#[cfg(unix)]
fn request(
    view: &Entity<HerdrWindow>,
    cx: &mut VisualTestContext,
    daemon: Option<&str>,
    workspace: Option<&str>,
    strict: bool,
) -> Option<Placed> {
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            let target = Target {
                daemon: daemon.map(std::path::Path::new),
                workspace,
                pane: Some("w0:p1"),
            };
            view.open_requested_browser_tab(
                &target,
                strict,
                &url("http://localhost:3000/"),
                false,
                window,
                cx,
            )
        })
    })
}

#[cfg(unix)]
#[gpui::test]
fn requests_open_tabs_only_in_a_workspace_the_window_shows(cx: &mut gpui::TestAppContext) {
    let (view, cx) = window(cx);
    let socket = view.read_with(cx, |view, _| {
        view.endpoints[0]
            .connection
            .target
            .socket_path()
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    });
    // No named workspace: the one the window shows.
    assert!(matches!(
        request(&view, cx, None, None, true),
        Some(Placed::Opened { workspace_id }) if workspace_id == "w0"
    ));
    assert!(matches!(
        request(&view, cx, None, Some("w1"), true),
        Some(Placed::Opened { workspace_id }) if workspace_id == "w1"
    ));
    assert!(request(&view, cx, None, Some("w_missing"), true).is_none());
    // Another daemon's socket never matches strictly, but its workspace ID
    // still finds the window once socket spellings are ignored.
    assert!(
        request(
            &view,
            cx,
            Some("/elsewhere/herdr-client.sock"),
            Some("w0"),
            true
        )
        .is_none()
    );
    assert!(
        request(
            &view,
            cx,
            Some("/elsewhere/herdr-client.sock"),
            Some("w0"),
            false
        )
        .is_some()
    );
    assert!(request(&view, cx, Some("/elsewhere/herdr-client.sock"), None, false).is_none());
    if let Some(socket) = socket {
        assert!(request(&view, cx, Some(&socket), Some("w0"), true).is_some());
    }
    // Opened without focus: the terminal stays in front.
    draw(cx);
    assert!(cx.debug_bounds("terminal").is_some());
    assert!(cx.debug_bounds("browser-tab-0").is_some());
    // The same pane showing the same page again got its tab back each time:
    // one tab in w0 and one in w1.
    cx.update(|_, cx| {
        let store = cx.global::<Store>();
        assert_eq!(store.opened_by("w0:p1").count(), 2);
    });
}

#[gpui::test]
fn tabs_of_a_closed_workspace_are_forgotten_but_a_restart_keeps_them(
    cx: &mut gpui::TestAppContext,
) {
    let (view, cx) = window(cx);
    let tab_scope = view.read_with(cx, |view, _| scope(&view.endpoints[0]));
    let open = |cx: &mut VisualTestContext, workspace: &str| {
        cx.update(|_, cx| {
            Store::update(cx, |store| {
                store.open(
                    tab_scope.clone(),
                    workspace,
                    Some(url("https://a.test/")),
                    None,
                )
            })
            .unwrap()
        })
    };
    let (kept, closed) = (open(cx, "w0"), open(cx, "w2"));
    let exists =
        |cx: &mut VisualTestContext, id| cx.update(|_, cx| cx.global::<Store>().get(id).is_some());
    let poll = |view: &Entity<HerdrWindow>, cx: &mut VisualTestContext, boot: &str, count| {
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                let mut next = snapshot(count);
                next.boot_id = boot.into();
                next.focused_workspace_id = Some("w0".into());
                view.live.snapshot = Some(Arc::new(next));
                view.poll_browser(window, cx);
            })
        });
    };
    poll(&view, cx, "boot-1", 3);
    // A daemon that restarted without w2 proves nothing about w2.
    poll(&view, cx, "boot-2", 2);
    assert!(exists(cx, closed));
    poll(&view, cx, "boot-2", 3);
    // The same daemon dropping w2 means it was closed.
    poll(&view, cx, "boot-2", 2);
    assert!(!exists(cx, closed));
    assert!(exists(cx, kept));
}

/// Notes need a page to annotate, which Linux builds do not show.
#[cfg(any(target_os = "macos", windows))]
mod notes {
    use super::*;

    const PICK: &str = r##"{"kind":"pick","target":{"kind":"element","selector":"#save","tag":"button","text":"Save","html":"<button id=\"save\">Save</button>"}}"##;

    /// A snapshot where pane `w0:p1` runs an agent with `status`.
    fn with_agent(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext, status: &str) {
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                let mut shown: serde_json::Value =
                    serde_json::to_value(view.live.snapshot.as_deref().unwrap()).unwrap();
                shown["panes"] = serde_json::json!([{
                    "pane_id": "w0:p1", "workspace_id": "w0", "tab_id": "t0", "label": null,
                    "cwd": null, "foreground_cwd": null, "focused": true,
                    "right_click_passthrough": false
                }]);
                shown["agents"] = serde_json::json!([{
                    "pane_id": "w0:p1", "workspace_id": "w0", "tab_id": "t0", "name": "claude",
                    "display_agent": "Claude Code", "agent": "claude", "title": null,
                    "terminal_title": null, "terminal_title_stripped": null,
                    "agent_status": status, "state_change_seq": 0, "state_labels": [],
                    "tokens": [], "focused": true
                }]);
                view.live.snapshot = Some(Arc::new(serde_json::from_value(shown).unwrap()));
            });
        });
    }

    /// Opens a page the way an agent in pane `w0:p1` would, and writes a note on
    /// its Save button.
    fn noted_tab(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext) -> crate::browser::Tab {
        let tab_scope = view.read_with(cx, |view, _| scope(&view.endpoints[0]));
        let tab = cx.update(|_, cx| {
            let id = Store::update(cx, |store| {
                store.open(
                    tab_scope,
                    "w0",
                    Some(url("http://localhost:3000/")),
                    Some("w0:p1".into()),
                )
            })
            .unwrap();
            cx.global::<Store>().get(id).cloned().unwrap()
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                // A page's posts are ignored until the user starts annotating.
                view.page_posted(tab.id, PICK, window, cx);
                assert!(!view.browser.annotations.open(tab.id));
                view.toggle_annotating(tab.id, window, cx);
                view.page_posted(tab.id, "not json", window, cx);
                view.page_posted(tab.id, PICK, window, cx);
                let input = view.browser.annotations.input.clone();
                input.update(cx, |input, cx| input.set_text_selected("Make it blue", cx));
                view.add_note(tab.id, window, cx);
                assert_eq!(view.browser.annotations.queued(tab.id), 1);
            });
        });
        tab
    }

    fn kept(cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| {
            cx.default_global::<crate::browser::Feedback>()
                .take("w0:p1")
        })
    }

    #[gpui::test]
    fn notes_reach_the_agent_that_opened_the_page(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);

        // The agent's pane is not in this window: the notes wait for it.
        let tab = noted_tab(&view, cx);
        cx.update(|_, cx| view.update(cx, |view, cx| view.send_notes(&tab, cx)));
        let text = kept(cx).unwrap();
        assert!(text.contains("On <button> at `#save`"), "{text}");
        assert!(text.contains("Note: Make it blue"), "{text}");
        assert!(kept(cx).is_none(), "taken once");

        // An agent waiting in `browser feedback --wait` gets them directly.
        let tab = noted_tab(&view, cx);
        cx.update(|_, cx| {
            cx.default_global::<crate::browser::Feedback>()
                .set_waiting(vec!["w0:p1".into()]);
            view.update(cx, |view, cx| view.send_notes(&tab, cx));
            cx.default_global::<crate::browser::Feedback>()
                .set_waiting(Vec::new());
        });
        assert!(kept(cx).is_some());

        // A working agent's pane is typed into once it is idle; this fixture has
        // no connection, so the paste fails and the notes are kept instead.
        with_agent(&view, cx, "working");
        let tab = noted_tab(&view, cx);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.send_notes(&tab, cx);
                assert_eq!(view.deliveries.len(), 1);
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 1, "held while busy");
            });
        });
        assert!(kept(cx).is_none());
        with_agent(&view, cx, "idle");
        cx.update(|_, cx| view.update(cx, |view, cx| view.poll_deliveries(cx)));
        view.read_with(cx, |view, _| {
            assert_eq!(view.deliveries.len(), 0);
            assert_eq!(view.browser.annotations.queued(tab.id), 0);
        });
        assert!(kept(cx).is_some_and(|text| text.contains("Make it blue")));
    }

    #[gpui::test]
    fn a_pane_without_an_agent_is_never_typed_into(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        with_agent(&view, cx, "idle");
        // The agent exited: its pane is back at a shell, where Enter would
        // run the pasted notes.
        cx.update(|_, cx| {
            view.update(cx, |view, _| {
                let mut shown = (*view.live.snapshot.clone().unwrap()).clone();
                shown.agents.clear();
                view.live.snapshot = Some(Arc::new(shown));
            });
        });
        let tab = noted_tab(&view, cx);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.send_notes(&tab, cx);
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 0);
            });
        });
        assert!(kept(cx).is_some_and(|text| text.contains("Make it blue")));
    }

    #[gpui::test]
    fn an_agent_asking_a_question_is_not_typed_into(cx: &mut gpui::TestAppContext) {
        let (view, cx) = window(cx);
        with_agent(&view, cx, "blocked");
        let tab = noted_tab(&view, cx);
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.send_notes(&tab, cx);
                // Deadline not reached: still held.
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 1);
            });
        });
        // The pane closing sends them to `browser feedback` rather than nowhere.
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let mut shown = (*view.live.snapshot.clone().unwrap()).clone();
                shown.panes.clear();
                shown.agents.clear();
                view.live.snapshot = Some(Arc::new(shown));
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 0);
            });
        });
        assert!(kept(cx).is_some());
    }
}
