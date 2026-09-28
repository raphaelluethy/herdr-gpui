use super::{Page, WorkspaceMenuAction};
use crate::{
    HerdrWindow,
    pull_request::{Input, Origin, repository_input},
};
use gpui::{prelude::*, *};
use herdr_client::protocol::*;
use std::sync::Arc;

impl HerdrWindow {
    /// Where the selected device's checkouts live, if pull requests can be
    /// looked up for them: the verified local daemon, or a saved SSH device.
    /// Any other socket may be forwarded from an unknown machine.
    pub(crate) fn pr_origin(&self) -> Option<Origin> {
        match &self.endpoints[self.selected_endpoint].connection.target {
            herdr_client::ConnectTarget::Ssh { target, .. } => Some(Origin::Ssh(target.clone())),
            _ if self.selected_endpoint == 0 && self.live.local_daemon_peer => Some(Origin::Local),
            _ => None,
        }
    }

    pub(super) fn refresh_workspace_pr(&mut self) {
        self.menu.pr.clear();
        if !self.has_forge_access() {
            return;
        }
        let result = (|| {
            let target = self
                .menu
                .target
                .as_ref()
                .ok_or(crate::Error::StaleWorkspace)?;
            if target.worktree.is_none() || target.branch.as_deref().is_none_or(str::is_empty) {
                return Err(crate::Error::PrMetadata);
            }
            if self.pr_origin().is_none() {
                return Err(crate::Error::PrUntrustedEndpoint);
            }
            if !self.workspace_pr_target_current() {
                return Err(crate::Error::StaleWorkspace);
            }
            repository_input(target.worktree.as_ref(), target.branch.as_deref())
        })();
        match result {
            Ok(input) => {
                self.sync_pr_scope();
                self.menu.pr_connection = Some(Arc::downgrade(
                    &self.endpoints[self.selected_endpoint].connection.inbox,
                ));
                self.menu
                    .pr_cache
                    .present(&input, &mut self.menu.pr, std::time::Instant::now());
            }
            Err(error) => self.menu.pr.message = Some(error.to_string()),
        }
    }

    pub(super) fn sync_pr_scope(&mut self) {
        let endpoint = &self.endpoints[self.selected_endpoint];
        let same_connection = self
            .menu
            .pr_cache_connection
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|old| Arc::ptr_eq(&old, &endpoint.connection.inbox));
        if !same_connection {
            self.menu.pr_cache.clear();
            self.menu.pr_cache_connection = Some(Arc::downgrade(&endpoint.connection.inbox));
        }
        let forges = self.forges();
        if let (Some(snapshot), false, Some(origin)) =
            (&self.live.snapshot, forges.is_empty(), self.pr_origin())
        {
            let scope = (
                self.selection_epoch,
                self.endpoints[self.selected_endpoint].generation,
                snapshot.boot_id.clone(),
            );
            self.menu.pr_cache.scope(scope, forges, origin);
        }
    }

    fn workspace_pr_target_current(&self) -> bool {
        self.menu_target_current()
            && self.live.status.is_connected()
            && self.menu.pr_connection.as_ref().is_none_or(|old| {
                old.upgrade().is_some_and(|old| {
                    Arc::ptr_eq(
                        &old,
                        &self.endpoints[self.selected_endpoint].connection.inbox,
                    )
                })
            })
            && self.menu.target.as_ref().is_some_and(|target| {
                self.live.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.boot_id == target.boot_id
                        && snapshot.workspaces.iter().any(|workspace| {
                            workspace.workspace_id == target.id
                                && workspace.worktree == target.worktree
                                && workspace.branch == target.branch
                        })
                })
            })
    }

    pub(crate) fn update_workspace_pr(&mut self) -> bool {
        let mut changed = false;
        if !self.has_forge_access() {
            self.menu.pr_cache.clear();
            self.menu.pr.clear();
            if self.menu.workspace_selected == Some(WorkspaceMenuAction::PullRequest) {
                self.menu.workspace_selected = None;
                changed = true;
            }
            return changed;
        }
        let eligible = self.pr_origin().is_some()
            && self.live.status.is_connected()
            && self.live.snapshot.is_some();
        if eligible {
            self.sync_pr_scope();
            let now = std::time::Instant::now();
            if let Some(snapshot) = &self.live.snapshot {
                if self
                    .menu
                    .pr_snapshot
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .is_none_or(|old| !Arc::ptr_eq(&old, snapshot))
                {
                    self.menu.pr_snapshot = Some(Arc::downgrade(snapshot));
                    self.menu.pr_cache.retain(|input| {
                        snapshot.workspaces.iter().any(|workspace| {
                            workspace
                                .worktree
                                .as_ref()
                                .is_some_and(|tree| tree.key == input.repo_key)
                                && workspace.branch.as_ref() == Some(&input.branch)
                        })
                    });
                }
                if self.menu.pr_cache.scan_due(now) {
                    let priority = self
                        .menu
                        .target
                        .as_ref()
                        .filter(|_| self.menu.page == Some(Page::Workspace))
                        .map(|target| target.id.as_str());
                    let inputs = workspace_pr_inputs(snapshot, priority, self.menu.pr_cache.cursor);
                    self.menu.pr_cache.schedule(inputs, now);
                }
            }
            changed |= self.menu.pr_cache.poll(now);
        } else {
            self.menu.pr_cache.clear();
        }
        if self.menu.page == Some(Page::Workspace) && !self.workspace_pr_target_current() {
            if self.menu.pr.message.as_deref()
                != Some("Workspace changed or disconnected. Reopen the menu.")
            {
                self.menu.pr.clear();
                self.menu.pr.message =
                    Some("Workspace changed or disconnected. Reopen the menu.".into());
                changed = true;
            }
        } else if changed && self.menu.page == Some(Page::Workspace) {
            self.refresh_workspace_pr();
        }
        if self.menu.pr.value.is_none()
            && self.menu.workspace_selected == Some(WorkspaceMenuAction::PullRequest)
        {
            self.menu.workspace_selected = None;
            changed = true;
        }
        changed
    }

    pub(super) fn open_workspace_pr(&self, cx: &mut Context<Self>) {
        if self.has_forge_access()
            && self.workspace_pr_target_current()
            && let Some(pr) = &self.menu.pr.value
        {
            cx.open_url(&pr.url);
        }
    }

    pub(super) fn render_workspace_pr(&self, text_width: Pixels, cx: &mut Context<Self>) -> Div {
        let theme = &self.theme;
        let pr = &self.menu.pr;
        let mut section = div()
            .debug_selector(|| "workspace-pr".into())
            .mt(px(6.))
            .pt(px(4.))
            .pb(px(6.))
            .border_t_1()
            .border_color(rgb(theme.active))
            .flex_none()
            .min_w_0();
        if let Some(value) = &pr.value {
            let action = WorkspaceMenuAction::PullRequest;
            let color = value.color(theme);
            section =
                section
                    .child(
                        div()
                            .id("workspace-pr-title")
                            .debug_selector(|| "workspace-pr-title".into())
                            .px(px(8.))
                            .py(px(6.))
                            .flex_none()
                            .rounded(px(crate::config::corners::CONTROL))
                            .cursor_pointer()
                            .when(self.menu.workspace_selected == Some(action), |row| {
                                row.bg(rgb(theme.active))
                            })
                            .on_hover(cx.listener(move |this, hovered, _, cx| {
                                if *hovered {
                                    this.menu.workspace_selected = Some(action);
                                } else if this.menu.workspace_selected == Some(action) {
                                    this.menu.workspace_selected = None;
                                }
                                cx.notify();
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.activate_workspace_menu(action, window, cx);
                            }))
                            .child(
                                div()
                                    .w(text_width)
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(crate::sidebar::label_text(&format!(
                                        "#{} {}",
                                        value.number, value.title
                                    ))),
                            ),
                    )
                    .child(
                        div()
                            .ml(px(8.))
                            .w(text_width)
                            .flex_none()
                            .truncate()
                            .text_size(px((self.config.ui.size - 1.).max(8.)))
                            .text_color(rgb(theme.muted))
                            .child(format!(
                                "{} -> {}",
                                value.head_ref_name, value.base_ref_name
                            )),
                    )
                    .child(
                        div()
                            .mx(px(8.))
                            .mt(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_none()
                                    .px(px(6.))
                                    .rounded(px(crate::config::corners::CONTROL))
                                    .bg(rgba((color << 8) | 0x20))
                                    .text_color(rgb(color))
                                    .child(value.lifecycle()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_color(rgb(theme.muted))
                                    .child(value.review()),
                            ),
                    )
                    .child(
                        div()
                            .mx(px(8.))
                            .mt(px(4.))
                            .text_color(rgb(theme.muted))
                            .child(value.merge_status()),
                    )
                    .child(
                        div()
                            .mx(px(8.))
                            .mt(px(4.))
                            .child(value.checks_summary.clone()),
                    )
                    .child(
                        div()
                            .mx(px(8.))
                            .mt(px(6.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(div().text_color(rgb(theme.palette[2])).child(
                                crate::sidebar::label_text(&format!("+{}", value.additions)),
                            ))
                            .child(div().text_color(rgb(theme.palette[1])).child(
                                crate::sidebar::label_text(&format!("-{}", value.deletions)),
                            ))
                            .child(div().text_color(rgb(theme.muted)).child(format!(
                                "{} {}",
                                value.changed_files,
                                if value.changed_files == 1 {
                                    "file"
                                } else {
                                    "files"
                                }
                            ))),
                    );
        }
        if pr.loading {
            section = section.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_color(rgb(theme.muted))
                    .child("Checking GitHub..."),
            );
        } else if let Some(message) = &pr.message {
            section = section.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_color(rgb(theme.muted))
                    .child(format!(
                        "{}{message}",
                        if pr.value.is_some() { "Stale: " } else { "" }
                    )),
            );
        } else if pr.value.is_none() {
            section = section.child(
                div()
                    .px(px(8.))
                    .py(px(4.))
                    .text_color(rgb(theme.muted))
                    .child("No PR found for this origin and branch."),
            );
        }
        section
    }

    #[cfg(any(test, all(feature = "integration-test", target_os = "macos")))]
    pub(crate) fn workspace_pr_fixture(
        &mut self,
        value: crate::pull_request::PullRequest,
    ) -> crate::Result<()> {
        let target = self
            .menu
            .target
            .as_ref()
            .ok_or(crate::Error::StaleWorkspace)?;
        let input = repository_input(target.worktree.as_ref(), target.branch.as_deref())?;
        self.menu.github = crate::github::Auth::connected_fixture();
        self.live.local_daemon_peer = true;
        self.sync_pr_scope();
        self.menu
            .pr_cache
            .seed(input, value, std::time::Instant::now());
        self.refresh_workspace_pr();
        Ok(())
    }

    #[cfg(any(test, feature = "integration-test"))]
    pub(crate) fn github_fixture(
        &mut self,
        waiting: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_menu(window, cx);
        self.menu.page = Some(Page::GitHub);
        self.menu.github = crate::github::Auth::fixture(waiting);
        cx.notify();
    }
}

#[cfg(test)]
fn checkout_input(
    response: &serde_json::Value,
    id: &str,
    worktree: Option<&ClientShellWorktree>,
    branch: Option<&str>,
) -> crate::Result<Input> {
    let result = &response["result"];
    let workspace = &result["workspace"];
    let tree = &workspace["worktree"];
    let mut input = repository_input(worktree, branch)?;
    if response.get("error").is_some()
        || result["type"] != "workspace_info"
        || workspace["workspace_id"] != id
        || tree["repo_key"] != input.repo_key
    {
        return Err(crate::Error::WorkspaceCheckoutChanged);
    }
    let checkout = tree["checkout_path"]
        .as_str()
        .filter(|s| std::path::Path::new(s).is_absolute())
        .ok_or(crate::Error::PrAbsolutePath)?;
    input.checkout = Some(checkout.into());
    Ok(input)
}

fn workspace_pr_inputs<'a>(
    snapshot: &'a ClientShellSnapshot,
    open: Option<&'a str>,
    cursor: usize,
) -> impl Iterator<Item = Input> + 'a {
    let count = snapshot.workspaces.len();
    let start = cursor % count.max(1);
    let priority = open.or(snapshot.focused_workspace_id.as_deref());
    // Alternate priority and round-robin admission so focus changes cannot
    // starve the rest of the live workspace list. Cache scheduling deduplicates.
    snapshot
        .workspaces
        .iter()
        .filter(move |workspace| {
            cursor.is_multiple_of(2) && Some(workspace.workspace_id.as_str()) == priority
        })
        .chain(snapshot.workspaces.iter().cycle().skip(start).take(count))
        .filter_map(|workspace| {
            repository_input(workspace.worktree.as_ref(), workspace.branch.as_deref()).ok()
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{checkout_input, repository_input};
    use herdr_client::protocol::ClientShellWorktree;

    #[test]
    fn metadata_priority_alternates_with_round_robin_and_skips_ineligible_workspaces() {
        let mut snapshot = crate::sidebar::layout_tests::snapshot(6);
        snapshot.focused_workspace_id = Some("w5".into());
        let branches = |snapshot: &super::ClientShellSnapshot, open, cursor| {
            super::workspace_pr_inputs(snapshot, open, cursor)
                .map(|input| input.branch)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            branches(&snapshot, None, 0)[0],
            "worktree/sidebar-child-with-a-long-readable-branch-name"
        );
        assert_eq!(
            branches(&snapshot, Some("w4"), 0)[0],
            "worktree/sidebar-child"
        );
        assert_eq!(
            branches(&snapshot, Some("w4"), 1)[0],
            "develop",
            "every other admission serves the rest"
        );
        assert_eq!(
            branches(&snapshot, None, 5)[0],
            "worktree/sidebar-child-with-a-long-readable-branch-name"
        );
        snapshot.workspaces[3].worktree.as_mut().unwrap().key = "relative".into();
        snapshot.workspaces[4].branch = None;
        assert_eq!(branches(&snapshot, None, 1).len(), 1);
        snapshot.workspaces.clear();
        assert!(branches(&snapshot, None, 0).is_empty());
    }

    #[gpui::test]
    fn cached_menu_open_is_immediate_and_does_not_touch_deletion_response(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.live.status = crate::state::ConnectionStatus::Connected;
                view.open_workspace_menu("w3", Default::default(), window, cx);
                view.workspace_pr_fixture(crate::pull_request::fixture().unwrap())
                    .unwrap();
                view.dismiss_menu(window, cx);
                let pending = Some(("delete-request".into(), None));
                view.live.dialog_response = pending.clone();
                view.endpoints[0]
                    .connection
                    .inbox
                    .lock()
                    .unwrap()
                    .dialog_response = pending.clone();
                view.open_workspace_menu("w3", Default::default(), window, cx);
                assert_eq!(view.menu.pr.value.as_ref().unwrap().number, 8);
                assert!(!view.menu.pr.loading);
                assert_eq!(view.menu.workspace_selected, None);
                assert!(
                    matches!(&view.live.dialog_response, Some((id, None)) if id == "delete-request")
                );
                assert!(matches!(
                    &view.endpoints[0]
                        .connection
                        .inbox
                        .lock()
                        .unwrap()
                        .dialog_response,
                    Some((id, None)) if id == "delete-request"
                ));
                // Switching repository/branch never reuses this cached PR.
                view.dismiss_menu(window, cx);
                view.open_workspace_menu("w4", Default::default(), window, cx);
                assert!(view.menu.pr.value.is_none());
                assert!(view.menu.pr.loading);
            })
        });
    }

    #[gpui::test]
    fn compact_pr_is_the_only_metadata_action(cx: &mut gpui::TestAppContext) {
        use super::super::{Page, WorkspaceAction, WorkspaceMenuAction};
        use gpui::{point, px, size};
        let (view, cx) = cx.add_window_view(crate::sidebar::layout_tests::fixture_window);
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.live.status = crate::state::ConnectionStatus::Connected;
                view.open_workspace_menu("w3", point(px(620.), px(380.)), window, cx);
                view.menu.github = crate::github::Auth::connected_fixture();
                view.menu.pr.clear();
                view.menu.pr.loading = true;
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("down");
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.menu.pr.loading = false;
                view.menu.pr.value = Some(crate::pull_request::fixture().unwrap());
                assert_eq!(
                    view.menu.workspace_selected,
                    Some(WorkspaceMenuAction::Dialog(WorkspaceAction::Rename))
                );
                cx.notify();
            })
        });
        for width in [320., 640., 1200.] {
            cx.simulate_resize(size(px(width), px(400.)));
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
            let panel = cx.debug_bounds("menu-panel").unwrap();
            let title = cx.debug_bounds("workspace-pr-title").unwrap();
            let header = cx.debug_bounds("workspace-menu-header").unwrap();
            assert!(
                panel.size.height < px(340.) + header.size.height + px(4.),
                "oversized popover: {panel:?}"
            );
            assert!(panel.left() >= px(0.) && panel.right() <= px(width));
            assert!(panel.bottom() <= px(400.));
            assert!(panel.contains(&title.origin) && title.right() <= panel.right());
            assert!(cx.debug_bounds("workspace-pr-open").is_none());
            assert!(cx.debug_bounds("workspace-pr-refresh").is_none());
        }
        // No O/R aliases, and no default activation when the result arrives.
        cx.update(|_, cx| view.update(cx, |view, _| view.menu.workspace_selected = None));
        cx.simulate_keystrokes("enter o r");
        assert!(cx.opened_url().is_none());
        cx.simulate_keystrokes("up");
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).menu.workspace_selected,
                Some(WorkspaceMenuAction::PullRequest)
            );
        });
        // A stale target is rejected even before the asynchronous invalidation tick.
        cx.update(|_, cx| view.update(cx, |view, _| view.endpoints[0].generation += 1));
        cx.simulate_keystrokes("enter");
        assert!(cx.opened_url().is_none());
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                assert!(view.update_workspace_pr());
                assert_eq!(view.menu.workspace_selected, None);
                view.endpoints[0].generation -= 1;
                view.menu.pr.clear();
                view.menu.pr.value = Some(crate::pull_request::fixture().unwrap());
                cx.notify();
            })
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let title = cx.debug_bounds("workspace-pr-title").unwrap().center();
        let rename = cx.debug_bounds("workspace-menu-Rename").unwrap().center();
        cx.simulate_mouse_move(title, None, Default::default());
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).menu.workspace_selected,
                Some(WorkspaceMenuAction::PullRequest)
            )
        });
        cx.simulate_keystrokes("down");
        cx.update(|_, cx| {
            assert_eq!(
                view.read(cx).menu.workspace_selected,
                Some(WorkspaceMenuAction::Dialog(WorkspaceAction::Rename))
            )
        });
        cx.simulate_mouse_move(rename, None, Default::default());
        cx.simulate_keystrokes("up enter");
        let expected = cx.update(|_, cx| view.read(cx).menu.pr.value.as_ref().unwrap().url.clone());
        assert_eq!(cx.opened_url(), Some(expected.clone()));
        cx.simulate_click(title, Default::default());
        assert_eq!(cx.opened_url(), Some(expected));
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.menu.github = Default::default();
                view.update_workspace_pr();
                assert_eq!(view.menu.workspace_selected, None);
                assert!(
                    !view
                        .workspace_menu_actions()
                        .contains(&WorkspaceMenuAction::PullRequest)
                );
                assert!(view.menu.page == Some(Page::Workspace));
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        // GPUI retains removed debug selectors; measure the remaining action panel.
        // Five action rows and the target header: no PR section or stale metadata.
        let rows = cx.update(|_, cx| view.read(cx).workspace_menu_actions().len());
        assert_eq!(rows, 5);
        let panel = cx.debug_bounds("menu-panel").unwrap().size.height;
        let header = cx
            .debug_bounds("workspace-menu-header")
            .unwrap()
            .size
            .height
            + px(4.);
        assert!(panel - header < px(35. * rows as f32), "{panel:?}");
    }

    /// Explicitly selected running daemon only: no start, focus, resize, input,
    /// credential writes, or terminal surface subscription. Do not log metadata.
    #[cfg(all(feature = "integration-test", target_os = "macos"))]
    #[test]
    #[allow(clippy::expect_used)]
    #[ignore = "requires explicit HERDR_TEST_PR_SOCKET/REPO_KEY/BRANCH; HERDR_TEST_PR_GITHUB=1 additionally uses existing sign-in"]
    fn live_local_pr_lookup() {
        use herdr_client::{
            ClientEvent, ConnectOptions, ConnectTarget, Method, connect_with_connector,
        };
        use std::{
            env,
            path::PathBuf,
            time::{Duration, Instant},
        };

        let socket = env::var_os("HERDR_TEST_PR_SOCKET").expect("explicit PR socket required");
        let repo_key =
            env::var("HERDR_TEST_PR_REPO_KEY").expect("explicit repository key required");
        let branch = env::var("HERDR_TEST_PR_BRANCH").expect("explicit branch required");
        let client = connect_with_connector(
            ConnectTarget::Socket(PathBuf::from(socket)),
            ConnectOptions::default(),
            false,
            |target, stop| {
                let (stream, local) =
                    crate::daemon::connect(target, stop, || panic!("must not start daemon"))?;
                if !local {
                    return Err(std::io::Error::other("local endpoint validation failed"));
                }
                eprintln!("Live local endpoint validation passed.");
                Ok(stream)
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut supports_workspace_get = false;
        let snapshot = loop {
            let event = client
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("snapshot deadline");
            match event {
                ClientEvent::Connected(welcome) => {
                    supports_workspace_get = welcome
                        .methods
                        .iter()
                        .any(|method| method == Method::WorkspaceGet.as_str())
                }
                ClientEvent::Snapshot(snapshot) => break snapshot,
                ClientEvent::Disconnected { reason } => panic!("local connection failed: {reason}"),
                _ => {}
            }
        };
        let workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| {
                workspace.branch.as_deref() == Some(&branch)
                    && workspace
                        .worktree
                        .as_ref()
                        .is_some_and(|tree| tree.key == repo_key)
            })
            .expect("requested repository/branch not present in daemon snapshot");
        let input = if supports_workspace_get {
            let id = client
                .handle
                .request(
                    &snapshot.boot_id,
                    Method::WorkspaceGet,
                    serde_json::json!({"workspace_id":workspace.workspace_id}),
                )
                .unwrap();
            let response = loop {
                let event = client
                    .events
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .expect("workspace response deadline");
                match event {
                    ClientEvent::Response {
                        request_id,
                        response,
                    } if request_id == id => break response,
                    ClientEvent::Disconnected { reason } => {
                        panic!("read-only workspace request failed: {reason}");
                    }
                    ClientEvent::CommandRejected { reason, .. } => {
                        panic!("read-only workspace request failed: {reason}");
                    }
                    _ => {}
                }
            };
            checkout_input(
                &response,
                &workspace.workspace_id,
                workspace.worktree.as_ref(),
                workspace.branch.as_deref(),
            )
            .unwrap()
        } else {
            eprintln!("Using validated Git worktree registry for older daemon.");
            repository_input(workspace.worktree.as_ref(), workspace.branch.as_deref()).unwrap()
        };
        client.handle.disconnect();
        crate::pull_request::local_repository(
            &input,
            &crate::forge::Forges::default(),
            Instant::now() + Duration::from_secs(15),
            &|| false,
        )
        .unwrap();
        eprintln!("Live checkout common-directory, branch, and GitHub origin validation passed.");
        if env::var_os("HERDR_TEST_PR_GITHUB").as_deref() != Some(std::ffi::OsStr::new("1")) {
            eprintln!("Authenticated GitHub lookup not requested (set HERDR_TEST_PR_GITHUB=1).");
            return;
        }
        let mut auth = crate::github::Auth::default();
        auth.initialize(&crate::config::Config::default());
        let deadline = Instant::now() + Duration::from_secs(45);
        while auth.loading_profile() && Instant::now() < deadline {
            auth.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        let profile = auth
            .profile
            .as_ref()
            .expect("existing GitHub sign-in unavailable");
        let mut lookup = crate::pull_request::Lookup::default();
        lookup.request(
            input,
            crate::pull_request::Origin::Local,
            crate::forge::Forges {
                github: Some(crate::forge::Access::Native(profile.token.clone())),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while lookup.loading && Instant::now() < deadline {
            lookup.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!lookup.loading, "PR worker deadline");
        assert!(
            lookup.message.is_none(),
            "PR lookup failed: {:?}",
            lookup.message
        );
        assert!(lookup.checked.is_some());
        eprintln!(
            "Live local endpoint, checkout identity/branch, and authenticated PR lookup passed (PR present: {}).",
            lookup.value.is_some()
        );
    }

    #[test]
    fn checkout_response_requires_authoritative_identity_and_absolute_path() {
        let root = std::env::temp_dir();
        let repo_key = root.join("repo/.git").to_str().unwrap().to_owned();
        let checkout = root.join("worktree").to_str().unwrap().to_owned();
        let tree = ClientShellWorktree {
            key: repo_key.clone(),
            label: "repo".into(),
            is_linked_worktree: true,
        };
        let response = serde_json::json!({"result":{"type":"workspace_info", "workspace":{
            "workspace_id":"w", "worktree":{"repo_key":repo_key, "checkout_path":checkout}
        }}});
        let input = checkout_input(&response, "w", Some(&tree), Some("feature")).unwrap();
        assert_eq!(input.checkout.as_deref(), Some(checkout.as_str()));
        assert_eq!(input.repo_key, repo_key);
        assert_eq!(input.branch, "feature");
        let fallback = repository_input(Some(&tree), Some("feature")).unwrap();
        assert!(fallback.checkout.is_none());
        assert_eq!(fallback.repo_key, input.repo_key);
        assert_eq!(fallback.branch, input.branch);
        assert!(checkout_input(&response, "wrong", Some(&tree), Some("feature")).is_err());
        for branch in [None, Some(""), Some("bad\nbranch")] {
            assert!(checkout_input(&response, "w", Some(&tree), branch).is_err());
        }
        let mut bad = response.clone();
        bad["result"]["workspace"]["worktree"]["checkout_path"] = "relative".into();
        assert!(checkout_input(&bad, "w", Some(&tree), Some("feature")).is_err());
        let mut bad = response.clone();
        bad["result"]["workspace"]["worktree"]["repo_key"] = "/other/.git".into();
        assert!(checkout_input(&bad, "w", Some(&tree), Some("feature")).is_err());
        let mut bad = response;
        bad["error"] = serde_json::json!({"code":"unsupported"});
        assert!(checkout_input(&bad, "w", Some(&tree), Some("feature")).is_err());
    }
}
