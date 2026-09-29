//! The window side of browser tabs: their entries in the tab strip, the page
//! and its toolbar in a group where the terminal would be, and requests to
//! open one.
#[cfg(any(target_os = "macos", windows))]
use super::Annotations;
use super::{
    Location, Scope, Store, Tab, TabId, WebUrl,
    groups::{GroupId, GroupIds, Layout, Pick, SavedLayout, Slot},
};
use crate::{HerdrWindow, search_input::SearchInput, window::Flash};
#[cfg(unix)]
use crate::{
    NavigationTarget,
    control::{Placed, Target},
};
use gpui::{prelude::*, *};
use std::collections::{HashMap, HashSet};

/// One window's browser state. How each workspace's view is split, and what
/// each group shows, is the window's own choice, like its focused workspace;
/// the tabs themselves are the app's.
pub(crate) struct Browser {
    /// Per workspace, its groups. A workspace missing here has one group
    /// following its terminal.
    pub(crate) layouts: HashMap<(Scope, String), Layout>,
    /// Layouts saved before this window opened, restored the first time it
    /// shows each workspace.
    pub(crate) saved: HashMap<(Scope, String), SavedLayout>,
    /// The workspace whose layout was just restored, whose group in use is
    /// taken back to its tab once, rather than following wherever the
    /// daemon's focus was left.
    pub(crate) restored: Option<(Scope, String)>,
    pub(super) group_ids: GroupIds,
    /// The group drawn before the window has a workspace to split.
    pub(super) fallback_group: GroupId,
    #[cfg(any(target_os = "macos", windows))]
    pub(super) pages: super::Pages,
    /// Where each page drew last frame, to tell which ones a menu covers.
    #[cfg(any(target_os = "macos", windows))]
    page_bounds: std::rc::Rc<std::cell::RefCell<HashMap<TabId, Bounds<Pixels>>>>,
    /// Pages a menu covers, stood in for by a picture of themselves.
    #[cfg(any(target_os = "macos", windows))]
    frozen: HashMap<TabId, Freeze>,
    /// Each group's address field, and the tab whose address it last showed.
    pub(super) addresses: HashMap<GroupId, (Entity<SearchInput>, Option<TabId>)>,
    /// The group whose "+" asked for a new Herdr tab, which it picks once
    /// the daemon focuses the tab.
    pub(super) new_tab_group: Option<((Scope, String), GroupId)>,
    /// The connection of each group that shows a terminal.
    pub(crate) terminals: crate::group_terminals::GroupTerminals,
    /// Tabs growing into the strip as they open.
    pub(super) appear: super::tab_appear::TabAppear,
    /// Each strip's sideways scroll.
    pub(super) tab_scroll: super::tab_scroll::TabScroll,
    /// Groups opening from a split and folding away as they close.
    pub(super) group_motion: super::group_motion::GroupMotion,
    /// Why a tab's page could not be created, shown in its place.
    pub(super) failed: Option<(TabId, SharedString)>,
    /// The workspaces of the last snapshot and the boot they came from: one
    /// missing from the next snapshot of the same boot was closed.
    workspaces: Option<(Scope, String, HashSet<String>)>,
    #[cfg(any(target_os = "macos", windows))]
    pub(super) annotations: Annotations,
}

impl Browser {
    pub(crate) fn new(cx: &mut App) -> Self {
        let mut group_ids = GroupIds::default();
        Self {
            layouts: HashMap::new(),
            saved: super::Layouts::snapshot(cx),
            restored: None,
            fallback_group: group_ids.next(),
            group_ids,
            #[cfg(any(target_os = "macos", windows))]
            pages: Default::default(),
            #[cfg(any(target_os = "macos", windows))]
            page_bounds: Default::default(),
            #[cfg(any(target_os = "macos", windows))]
            frozen: HashMap::new(),
            addresses: HashMap::new(),
            new_tab_group: None,
            terminals: Default::default(),
            appear: Default::default(),
            tab_scroll: Default::default(),
            group_motion: Default::default(),
            failed: None,
            workspaces: None,
            #[cfg(any(target_os = "macos", windows))]
            annotations: Annotations::new(cx),
        }
    }
}

/// A page a menu covers. A native page sits above everything the window
/// draws, so it steps aside for the menu, leaving a picture of itself.
#[cfg(any(target_os = "macos", windows))]
enum Freeze {
    /// The picture was asked for, while the page still shows: WebKit only
    /// pictures a page on screen.
    Asked(std::time::Instant),
    /// Only macOS takes pictures.
    #[cfg(target_os = "macos")]
    Ready(std::sync::Arc<RenderImage>),
    /// No picture came in time, or this platform takes none.
    Blank,
}

/// How long a covered page may keep showing while its picture is taken and
/// decoded.
#[cfg(any(target_os = "macos", windows))]
const FREEZE_WAIT: std::time::Duration = std::time::Duration::from_millis(300);

/// Names the daemon behind an endpoint the way browser tabs remember it.
pub(crate) fn scope(endpoint: &crate::endpoint::Endpoint) -> Scope {
    match endpoint.connection.target.socket_path() {
        Ok(path) => Scope::local(&path),
        Err(_) => Scope::endpoint(&endpoint.id),
    }
}

/// Page titles run long; a tab shows the start of one, like a web browser.
pub(super) fn tab_label(title: &str) -> SharedString {
    const MAX_CHARS: usize = 28;
    match title.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => format!("{}\u{2026}", title[..end].trim_end()).into(),
        None => title.to_owned().into(),
    }
}

pub(super) fn store(cx: &App) -> Option<&Store> {
    cx.try_global::<Store>()
}

impl HerdrWindow {
    pub(crate) fn browser_key(&self) -> Option<(Scope, String)> {
        let workspace = self.live.snapshot.as_ref()?.focused_workspace_id.clone()?;
        Some((scope(&self.endpoints[self.selected_endpoint]), workspace))
    }

    /// The focused workspace's browser tabs, in the order they opened.
    pub(crate) fn browser_tab_ids(&self, cx: &App) -> Vec<TabId> {
        let (Some((scope, workspace)), Some(store)) = (self.browser_key(), store(cx)) else {
            return Vec::new();
        };
        store
            .in_workspace(&scope, &workspace)
            .map(|tab| tab.id)
            .collect()
    }

    pub(super) fn forget_browser_tabs(&mut self, mut gone: impl FnMut(TabId) -> bool) {
        #[cfg(any(target_os = "macos", windows))]
        {
            self.browser.frozen.retain(|id, _| !gone(*id));
            self.browser
                .page_bounds
                .borrow_mut()
                .retain(|id, _| !gone(*id));
        }
        let key = self.browser_key();
        let focused = self.focused_herdr_tab().map(str::to_owned);
        for (workspace, layout) in &mut self.browser.layouts {
            // Only the focused workspace's terminal is known here.
            let focused = focused
                .as_deref()
                .filter(|_| key.as_ref() == Some(workspace));
            let closed: Vec<Pick> = layout
                .picks()
                .filter(|pick| matches!(pick, Pick::Page(id) if gone(*id)))
                .cloned()
                .collect();
            for pick in closed {
                layout.replace(&pick, None, focused);
            }
        }
        #[cfg(any(target_os = "macos", windows))]
        self.browser.pages.retain(|id| !gone(id));
        if self
            .browser
            .failed
            .as_ref()
            .is_some_and(|(id, _)| gone(*id))
        {
            self.browser.failed = None;
        }
        #[cfg(any(target_os = "macos", windows))]
        let annotated: Vec<TabId> = self
            .browser
            .annotations
            .ids()
            .filter(|id| gone(*id))
            .collect();
        #[cfg(any(target_os = "macos", windows))]
        for id in annotated {
            self.browser.annotations.forget(id);
        }
    }

    /// Opens a tab in the focused workspace, or tells the user why not.
    pub(crate) fn open_browser_tab(
        &mut self,
        url: Option<WebUrl>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !super::EMBEDDED {
            match url {
                Some(url) => cx.open_url(url.as_str()),
                None => self.show_flash(Flash::warning("Browser tabs need macOS or Windows"), cx),
            }
            return;
        }
        let Some((scope, workspace)) = self.browser_key() else {
            self.show_flash(Flash::warning("Open a workspace first"), cx);
            return;
        };
        let location = url.map(|url| Location::Web { url });
        match Store::update(cx, |store| store.open(scope, &workspace, location, None)) {
            Some(id) => self.show_browser_tab(id, window, cx),
            None => self.show_flash(Flash::warning("Too many browser tabs are open"), cx),
        }
    }

    /// The endpoint and workspace a control request names, if this window
    /// shows it. `strict` requires the caller's own daemon; otherwise any
    /// endpoint showing the named workspace qualifies. Only the Unix control
    /// socket asks.
    #[cfg(unix)]
    fn browser_target(&self, target: &Target<'_>, strict: bool) -> Option<(usize, String)> {
        self.endpoints
            .iter()
            .enumerate()
            .find_map(|(index, endpoint)| {
                if strict {
                    let matches = match target.daemon {
                        Some(daemon) => {
                            endpoint.connection.target.socket_path().ok().as_deref() == Some(daemon)
                        }
                        None => index == self.selected_endpoint,
                    };
                    if !matches {
                        return None;
                    }
                } else if target.workspace.is_none() {
                    return None;
                }
                let live = if index == self.selected_endpoint {
                    &self.live
                } else {
                    &endpoint.live
                };
                let snapshot = live.snapshot.as_ref()?;
                let workspace = match target.workspace {
                    Some(id) => id,
                    None => snapshot.focused_workspace_id.as_deref()?,
                };
                snapshot
                    .workspaces
                    .iter()
                    .any(|candidate| candidate.workspace_id == workspace)
                    .then(|| (index, workspace.to_owned()))
            })
    }

    /// Opens the tab a control request asks for, if this window shows its
    /// workspace. `None` leaves the request to another window.
    #[cfg(unix)]
    pub(crate) fn open_requested_browser_tab(
        &mut self,
        target: &Target<'_>,
        strict: bool,
        location: &Location,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Placed> {
        let (index, workspace) = self.browser_target(target, strict)?;
        let scope = scope(&self.endpoints[index]);
        // An agent showing the same page again gets its tab back, reloaded,
        // rather than another tab for every revision.
        let origin = target.pane;
        let before =
            store(cx).and_then(|store| store.opened_before(&scope, &workspace, origin, location));
        let id = match before {
            Some(id) => {
                #[cfg(any(target_os = "macos", windows))]
                self.browser.pages.reload(id, cx);
                id
            }
            None => {
                let opened = Store::update(cx, |store| {
                    store.open(
                        scope,
                        &workspace,
                        Some(location.clone()),
                        origin.map(str::to_owned),
                    )
                });
                let Some(id) = opened else {
                    return Some(Placed::Full);
                };
                id
            }
        };
        if focus {
            let focused = self
                .live
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.focused_workspace_id.as_deref());
            if index != self.selected_endpoint {
                let endpoint = self.endpoints[index].id.clone();
                self.navigate_endpoint(&endpoint, NavigationTarget::Workspace(&workspace), cx);
            } else if focused != Some(workspace.as_str()) {
                self.navigate(NavigationTarget::Workspace(&workspace), cx);
            }
            // Recorded against the workspace, so the tab shows once the
            // navigation lands even if it is still in flight.
            self.show_browser_tab(id, window, cx);
        }
        cx.notify();
        Some(Placed::Opened {
            workspace_id: workspace,
        })
    }

    /// Reloads this window's pages for `tabs`, as an agent asks after
    /// editing a page it showed.
    #[cfg(unix)]
    pub(crate) fn reload_browser_tabs(&mut self, tabs: &[TabId], cx: &mut Context<Self>) {
        #[cfg(any(target_os = "macos", windows))]
        for id in tabs {
            self.browser.pages.reload(*id, cx);
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = (tabs, cx);
    }

    /// Applies page reports, drops tabs other windows closed, and forgets the
    /// tabs of workspaces the daemon closed. Runs on every window tick.
    pub(crate) fn poll_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(any(target_os = "macos", windows))]
        self.apply_page_events(window, cx);
        if let Some(store) = store(cx) {
            let gone: HashSet<TabId> = self
                .browser
                .layouts
                .values()
                .flat_map(Layout::picks)
                .filter_map(|pick| match pick {
                    Pick::Page(id) => Some(*id),
                    Pick::Herdr(_) => None,
                })
                .filter(|id| store.get(*id).is_none())
                .collect();
            #[cfg(any(target_os = "macos", windows))]
            let gone: HashSet<TabId> = gone
                .into_iter()
                .chain(
                    self.browser
                        .pages
                        .ids()
                        .filter(|id| store.get(*id).is_none()),
                )
                .collect();
            if !gone.is_empty() {
                self.forget_browser_tabs(|id| gone.contains(&id));
                cx.notify();
            }
        }
        self.forget_closed_workspaces(cx);
        self.forget_closed_herdr_tabs(cx);
        self.sync_addresses(false, window, cx);
    }

    fn forget_closed_workspaces(&mut self, cx: &mut Context<Self>) {
        let Some(snapshot) = self.live.snapshot.as_ref() else {
            return;
        };
        let scope = scope(&self.endpoints[self.selected_endpoint]);
        let unchanged = self
            .browser
            .workspaces
            .as_ref()
            .is_some_and(|(seen_scope, boot, seen)| {
                seen_scope == &scope
                    && boot == &snapshot.boot_id
                    && seen.len() == snapshot.workspaces.len()
                    && snapshot
                        .workspaces
                        .iter()
                        .all(|workspace| seen.contains(&workspace.workspace_id))
            });
        if unchanged {
            return;
        }
        let current: HashSet<String> = snapshot
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_id.clone())
            .collect();
        let previous = self.browser.workspaces.replace((
            scope.clone(),
            snapshot.boot_id.clone(),
            current.clone(),
        ));
        // A restarted daemon or another session proves nothing was closed.
        let Some((_, _, seen)) = previous
            .filter(|(seen_scope, boot, _)| seen_scope == &scope && boot == &snapshot.boot_id)
        else {
            return;
        };
        let closed: Vec<String> = seen.difference(&current).cloned().collect();
        if closed.is_empty() {
            return;
        }
        self.browser
            .layouts
            .retain(|(saved, workspace), _| saved != &scope || !closed.contains(workspace));
        super::Layouts::forget_workspaces(cx, &scope, &closed);
        if !store(cx).is_some_and(|store| store.has_workspaces(&scope, &closed)) {
            return;
        }
        Store::update(cx, |store| store.forget_workspaces(&scope, &closed));
        cx.notify();
    }

    #[cfg(any(target_os = "macos", windows))]
    fn apply_page_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use super::native::Event;
        let events: Vec<Event> = self.browser.pages.drain().collect();
        for event in events {
            match event {
                Event::Title(id, title) => {
                    Store::update(cx, |store| store.visited(id, None, Some(&title)));
                }
                // The field follows on the next sync, unless someone is
                // typing in it.
                Event::Loaded(id, url) => {
                    let visited = store(cx)
                        .and_then(|store| store.get(id))
                        .and_then(|tab| tab.location.as_ref()?.visited(&url));
                    if let Some(location) = visited {
                        Store::update(cx, |store| store.visited(id, Some(location), None));
                    }
                    self.page_loaded(id, cx);
                }
                Event::Posted(id, body) => self.page_posted(id, &body, window, cx),
                #[cfg(target_os = "macos")]
                Event::Captured(id, capture, tiff) => self.page_captured(id, capture, tiff, cx),
                #[cfg(target_os = "macos")]
                Event::Frozen(id, tiff) => self.page_frozen(id, tiff, cx),
                Event::NewWindow(id, url) => {
                    let parent = store(cx).and_then(|store| store.get(id)).cloned();
                    if let (Some(parent), Ok(url)) = (parent, WebUrl::try_from(url.as_str())) {
                        // The new tab opens where its opener shows.
                        let group = self.group_showing(&Pick::Page(id), cx);
                        let opened = Store::update(cx, |store| {
                            store.open(
                                parent.scope,
                                &parent.workspace_id,
                                Some(Location::Web { url }),
                                parent.origin,
                            )
                        });
                        if let Some(opened) = opened {
                            self.show_browser_tab_in(group, opened, window, cx);
                        }
                    }
                }
            }
        }
    }

    fn submit_address(
        &mut self,
        group: GroupId,
        id: TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.group_address(group, cx).read(cx).text().to_owned();
        let Ok(url) = WebUrl::from_typed(&text) else {
            self.show_flash(Flash::warning("Not an http or https address"), cx);
            return;
        };
        let location = Location::Web { url };
        Store::update(cx, |store| store.visited(id, Some(location.clone()), None));
        #[cfg(any(target_os = "macos", windows))]
        if self.browser.pages.contains(id) {
            self.browser.pages.load(id, &location, cx);
            self.browser.pages.focus(id, cx);
        }
        self.show_browser_tab_in(Some(group), id, window, cx);
    }

    /// Whether a notes panel or a note is still moving, so the window draws
    /// another frame. Only builds that show pages have either.
    pub(crate) fn annotations_moving(&self) -> bool {
        #[cfg(any(target_os = "macos", windows))]
        return self.browser.annotations.moving(std::time::Instant::now());
        #[cfg(not(any(target_os = "macos", windows)))]
        false
    }

    /// Shows or hides the native pages to match what the window draws. The
    /// pages sit above everything GPUI paints, so a page an open menu covers
    /// steps aside, leaving a picture of itself where it can; a page the menu
    /// does not reach keeps showing. Returns whether another frame is needed
    /// to settle, while a menu is first laid out or a picture is on its way.
    pub(crate) fn present_browser(&mut self, cx: &mut Context<Self>) -> bool {
        #[cfg(any(target_os = "macos", windows))]
        {
            use crate::menu::Cover;
            let live = self.live_pages(cx);
            if self.menu.page.is_none() {
                self.browser.frozen.clear();
                self.browser.pages.present(&live, cx);
                return false;
            }
            let cover = self.menu.cover.get();
            let bounds = self.browser.page_bounds.borrow().clone();
            let now = std::time::Instant::now();
            let mut settling = cover == Cover::Unknown;
            let mut shown = Vec::new();
            for id in live {
                let covered = cover.covers(bounds.get(&id).copied());
                if !covered {
                    shown.push(id);
                    continue;
                }
                match self.browser.frozen.get(&id) {
                    None => {
                        let asked = bounds
                            .get(&id)
                            .is_some_and(|page| self.browser.pages.freeze(id, page.size, cx));
                        let freeze = if asked {
                            shown.push(id);
                            settling = true;
                            Freeze::Asked(now)
                        } else {
                            Freeze::Blank
                        };
                        self.browser.frozen.insert(id, freeze);
                    }
                    Some(Freeze::Asked(since)) if now.duration_since(*since) < FREEZE_WAIT => {
                        shown.push(id);
                        settling = true;
                    }
                    Some(Freeze::Asked(_)) => {
                        self.browser.frozen.insert(id, Freeze::Blank);
                    }
                    #[cfg(target_os = "macos")]
                    Some(Freeze::Ready(_)) => {}
                    Some(Freeze::Blank) => {}
                }
            }
            self.browser.pages.present(&shown, cx);
            settling
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = cx;
            false
        }
    }

    /// Decodes a covered page's picture off the UI thread, and has the page
    /// step aside only once it can be painted in the same frame, so there is
    /// never an empty frame between the page and its picture. A picture that
    /// arrives after the menu closed, or after the wait ran out, is dropped.
    #[cfg(target_os = "macos")]
    fn page_frozen(&mut self, id: TabId, tiff: Option<Vec<u8>>, cx: &mut Context<Self>) {
        let waiting =
            move |this: &Self| matches!(this.browser.frozen.get(&id), Some(Freeze::Asked(_)));
        if !waiting(self) {
            return;
        }
        let Some(tiff) = tiff else {
            self.browser.frozen.insert(id, Freeze::Blank);
            cx.notify();
            return;
        };
        let frame = cx
            .background_executor()
            .spawn(async move { super::snapshot::frame(&tiff) });
        cx.spawn(async move |this, cx| {
            let frame = frame.await;
            this.update(cx, |this, cx| {
                if waiting(this) {
                    let freeze = frame.map_or(Freeze::Blank, |frame| {
                        Freeze::Ready(std::sync::Arc::new(frame))
                    });
                    this.browser.frozen.insert(id, freeze);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The picture a covered page left, while a menu is open.
    #[cfg(any(target_os = "macos", windows))]
    fn frozen_picture(&self, id: TabId) -> Option<std::sync::Arc<RenderImage>> {
        match self.browser.frozen.get(&id)? {
            #[cfg(target_os = "macos")]
            Freeze::Ready(image) if self.menu.page.is_some() => Some(image.clone()),
            _ => None,
        }
    }

    /// The workspace's browser tabs, after its Herdr tabs in a group's strip.
    pub(crate) fn browser_tab_entries(
        &self,
        slot: Slot,
        shown: Option<TabId>,
        cx: &mut Context<Self>,
    ) -> Vec<(TabId, Stateful<Div>)> {
        let (Some((scope, workspace)), Some(store)) = (self.browser_key(), store(cx)) else {
            return Vec::new();
        };
        let tabs: Vec<Tab> = store
            .in_workspace(&scope, &workspace)
            .filter(|tab| self.group_lists(slot.id, &Pick::Page(tab.id)))
            .cloned()
            .collect();
        tabs.into_iter()
            .map(|tab| {
                let id = tab.id;
                let (background, text) = self.tab_colors(shown == Some(id), slot.id);
                let tab = div()
                    .id(SharedString::from(format!("browser-tab-{id}")))
                    .debug_selector(move || slot.selector(&format!("browser-tab-{id}")))
                    .pl(px(10.))
                    .pr(px(3.))
                    .py(px(2.))
                    .min_w(px(crate::TAB_WIDTH))
                    .map(|tab| self.grow_tab(tab, slot.id, &Pick::Page(id)))
                    .border_r_1()
                    .border_color(rgb(self.theme.active))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .bg(rgb(background))
                    .text_color(rgb(text))
                    .child(
                        svg()
                            .path("icons/globe.svg")
                            .size(px(12.))
                            .flex_none()
                            .text_color(rgb(text)),
                    )
                    .child(tab_label(&tab.title))
                    .child(
                        div()
                            .id("close-browser-tab")
                            .debug_selector(move || {
                                slot.selector(&format!("close-browser-tab-{id}"))
                            })
                            .size(px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(crate::config::corners::CONTROL))
                            .hover(move |s| s.bg(rgba((text << 8) | 0x24)))
                            .child(
                                svg()
                                    .path("icons/close.svg")
                                    .size(px(12.))
                                    .text_color(rgb(text)),
                            )
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            // Split, the page closes in its group alone.
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if this.is_split() {
                                    this.close_in_group(slot.id, vec![Pick::Page(id)], window, cx);
                                } else {
                                    this.close_browser_tab(id, window, cx);
                                }
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.show_browser_tab_in(Some(slot.id), id, window, cx);
                    }));
                (id, tab)
            })
            .collect()
    }

    fn toolbar_button(
        &self,
        slot: Slot,
        id: &'static str,
        icon: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let color = if enabled {
            self.theme.foreground
        } else {
            self.theme.muted
        };
        div()
            .id(id)
            .debug_selector(move || slot.selector(id))
            .size(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(crate::config::corners::CONTROL))
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(self.theme.active)))
                    .on_click(cx.listener(move |this, _, window, cx| action(this, window, cx)))
            })
            .child(svg().path(icon).size(px(14.)).text_color(rgb(color)))
    }

    /// The page with its toolbar, drawn in a group where the terminal would
    /// be. `keyboard` marks the one element holding the window's focus
    /// handle when no terminal is drawn to hold it.
    pub(crate) fn render_browser(
        &mut self,
        slot: Slot,
        tab: &Tab,
        gap: f32,
        keyboard: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = tab.id;
        let loaded = tab.location.is_some();
        let external = match &tab.location {
            Some(Location::Web { url }) => Some(url.clone()),
            _ => None,
        };
        #[cfg(any(target_os = "macos", windows))]
        let (annotate_button, panel) = {
            let annotating = self.browser.annotations.armed(id);
            // The panel slides open and closed beside the page: its content
            // keeps its width and the edge moves.
            let shown = self
                .browser
                .annotations
                .panel_shown(id, std::time::Instant::now());
            let panel = (shown > 0.).then(|| {
                div()
                    .flex_none()
                    .h_full()
                    .w(px(super::annotate_view::ANNOTATIONS_WIDTH * shown))
                    .overflow_hidden()
                    .child(self.render_annotations(tab, cx))
                    .into_any_element()
            });
            let button = {
                // Annotating needs the page itself, so a blank tab or a
                // build without pages has nothing to annotate.
                let enabled = loaded && super::EMBEDDED;
                let color = if annotating {
                    self.theme.text_on(self.theme.primary_wash())
                } else if enabled {
                    self.theme.foreground
                } else {
                    self.theme.muted
                };
                div()
                    .id("browser-annotate")
                    .debug_selector(move || slot.selector("browser-annotate"))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .h(px(24.))
                    .px(px(6.))
                    .rounded(px(crate::config::corners::CONTROL))
                    .when(annotating, |button| {
                        button.bg(rgb(self.theme.primary_wash()))
                    })
                    .text_color(rgb(color))
                    .child(
                        svg()
                            .path("icons/pencil.svg")
                            .size(px(13.))
                            .text_color(rgb(color)),
                    )
                    .child("Annotate")
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(self.theme.active)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.toggle_annotating(id, window, cx);
                            }))
                    })
            };
            (Some(button), panel)
        };
        // Linux builds show no pages, so there is nothing to annotate.
        #[cfg(not(any(target_os = "macos", windows)))]
        let (annotate_button, panel): (Option<Stateful<Div>>, Option<AnyElement>) = (None, None);
        #[cfg(any(target_os = "macos", windows))]
        let page = self.browser.pages.page(id).cloned();
        #[cfg(not(any(target_os = "macos", windows)))]
        let page: Option<AnyView> = None;
        let failure = self
            .browser
            .failed
            .as_ref()
            .filter(|(failed, _)| *failed == id)
            .map(|(_, message)| message.clone());
        let placeholder: SharedString = match (&failure, loaded) {
            (Some(message), _) => format!("Could not show this page: {message}").into(),
            (None, false) => "Type an address above and press Return.".into(),
            (None, true) if !super::EMBEDDED => {
                "This build cannot show pages in the window; open it in the system browser.".into()
            }
            (None, true) => "Loading\u{2026}".into(),
        };
        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .px(px(6.))
            .py(px(4.))
            .bg(rgb(self.theme.surface))
            .border_b_1()
            .border_color(rgb(self.theme.active))
            .child(self.toolbar_button(
                slot,
                "browser-back",
                "icons/arrow-left.svg",
                loaded,
                cx,
                move |this, _, cx| {
                    #[cfg(any(target_os = "macos", windows))]
                    this.browser.pages.back(id, cx);
                    #[cfg(not(any(target_os = "macos", windows)))]
                    let _ = (this, cx);
                },
            ))
            .child(self.toolbar_button(
                slot,
                "browser-forward",
                "icons/arrow-right.svg",
                loaded,
                cx,
                move |this, _, cx| {
                    #[cfg(any(target_os = "macos", windows))]
                    this.browser.pages.forward(id, cx);
                    #[cfg(not(any(target_os = "macos", windows)))]
                    let _ = (this, cx);
                },
            ))
            .child(self.toolbar_button(
                slot,
                "browser-reload",
                "icons/refresh.svg",
                loaded,
                cx,
                move |this, _, cx| {
                    #[cfg(any(target_os = "macos", windows))]
                    this.browser.pages.reload(id, cx);
                    #[cfg(not(any(target_os = "macos", windows)))]
                    let _ = (this, cx);
                },
            ))
            .child(
                div()
                    .id("browser-address")
                    .debug_selector(move || slot.selector("browser-address"))
                    .flex_1()
                    .min_w_0()
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        match event.keystroke.key.as_str() {
                            "enter" => this.submit_address(slot.id, id, window, cx),
                            "escape" => {
                                let tab = store(cx).and_then(|store| store.get(id)).cloned();
                                this.sync_address(slot.id, tab.as_ref(), true, window, cx);
                                window.focus(&this.focus, cx);
                            }
                            _ => return,
                        }
                        cx.stop_propagation();
                    }))
                    .child(self.group_address(slot.id, cx)),
            )
            // A split shows the window's flash once, in the group in use.
            .children(
                self.flash
                    .as_ref()
                    .filter(|_| Some(slot.id) == self.active_group())
                    .map(|(flash, _)| {
                        div()
                            .flex_none()
                            .max_w(px(240.))
                            .truncate()
                            .text_color(rgb(flash.accent(&self.theme)))
                            .child(flash.text.clone())
                    }),
            )
            .children(annotate_button)
            .child(self.toolbar_button(
                slot,
                "browser-external",
                "icons/external.svg",
                external.is_some(),
                cx,
                move |_, _, cx| {
                    if let Some(url) = &external {
                        cx.open_url(url.as_str());
                    }
                },
            ));
        #[cfg(any(target_os = "macos", windows))]
        let (picture, bounds) = (self.frozen_picture(id), self.browser.page_bounds.clone());
        let content = match (page, &failure) {
            (Some(page), None) => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(page)
                // Where the page draws, so a menu hides only the pages it
                // covers; and, while one does, the page's picture in its place.
                .map(|content| {
                    #[cfg(any(target_os = "macos", windows))]
                    let content = content
                        .child(
                            canvas(
                                move |area, _, _| {
                                    bounds.borrow_mut().insert(id, area);
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0(),
                        )
                        .children(picture.map(|picture| {
                            img(picture)
                                .debug_selector(|| "page-picture".into())
                                .absolute()
                                .inset_0()
                                .size_full()
                        }));
                    content
                })
                .into_any_element(),
            _ => div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px_4()
                .text_color(rgb(self.theme.muted))
                .child(
                    div()
                        .debug_selector(move || slot.selector("browser-placeholder"))
                        .child(placeholder),
                )
                .into_any_element(),
        };
        div()
            .id(SharedString::from(slot.selector("browser")))
            .debug_selector(move || slot.selector("browser"))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .pl(px(gap))
            .bg(rgb(self.theme.background))
            // Keeps window shortcuts reachable while the page does not hold
            // the keyboard; nothing here types into a terminal. A focus
            // handle belongs to one element, so a drawn terminal keeps it.
            .when(keyboard, |browser| browser.track_focus(&self.focus))
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(div().flex().flex_col().flex_1().min_w_0().child(content))
                    .children(panel),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn long_titles_are_shortened_on_a_character_boundary() {
        assert_eq!(super::tab_label("Example Domain"), "Example Domain");
        let long = "Rust Programming Language — Official Site";
        assert_eq!(
            super::tab_label(long),
            "Rust Programming Language \u{2014}\u{2026}"
        );
        assert_eq!(super::tab_label(&"é".repeat(40)).chars().count(), 29);
    }
}
