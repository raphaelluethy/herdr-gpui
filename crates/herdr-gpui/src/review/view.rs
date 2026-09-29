//! The window's side of the review panel: following the focused checkout,
//! keyboard handling while the panel has focus, and the actions its controls
//! perform, from staging to sending the comments to an agent.
use super::{
    Mode, Source,
    agents::{self, Candidate},
    notes::{Subject, prompt},
    status::safe_relative,
};
use crate::{
    HerdrWindow,
    agent_delivery::{Delivery, Fallback},
    window::Flash,
};
use gpui::{ClipboardItem, Context, KeyDownEvent, Window};
use std::{path::PathBuf, time::Instant};

/// Where a listed file lives on disk, for opening it: only a safe relative
/// path under the checkout Git verified, and never a deleted file.
pub(crate) fn open_target(listing: &super::worker::Listing, path: &str) -> Option<PathBuf> {
    let entry = listing.entry(path)?;
    if entry.is_deleted() || !safe_relative(path) {
        return None;
    }
    Some(listing.root.join(path))
}

impl HerdrWindow {
    /// Follows the focused checkout and drains the worker. Called from the
    /// window's poll task, never from a render or input path.
    pub(crate) fn update_review(&mut self) -> bool {
        let now = Instant::now();
        let refresh = self.review.open && self.active;
        let source = self
            .git
            .tracked()
            .cloned()
            .map(Source::Worktree)
            .or_else(|| self.focused_directory().map(Source::Directory));
        let mut changed = self.review.track(source, refresh, now);
        changed |= self.review.poll(self.git.running().is_some(), now);
        changed
    }

    /// Where the process in the focused tab's pane works, on the owned local
    /// daemon only: Herdr keeps a worktree only for workspaces it created as
    /// one, so every other workspace is reviewed from its open tab.
    fn focused_directory(&self) -> Option<String> {
        if !self.local_git_endpoint() {
            return None;
        }
        let snapshot = self.live.snapshot.as_ref()?;
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| Some(&pane.pane_id) == snapshot.focused_pane_id.as_ref())
            .or_else(|| snapshot.panes.iter().find(|pane| pane.focused))?;
        pane.foreground_cwd
            .as_deref()
            .or(pane.cwd.as_deref())
            .filter(|path| std::path::Path::new(path).is_absolute())
            .map(str::to_owned)
    }

    pub(crate) fn toggle_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review.open = !self.review.open;
        if self.review.open {
            self.review.refresh();
            window.focus(&self.review.focus, cx);
        } else {
            self.review.cancel_draft();
            self.review.picker_open = false;
            window.focus(&self.focus, cx);
        }
        cx.notify();
    }

    pub(crate) fn set_review_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.review.set_mode(mode);
        cx.notify();
    }

    /// Selects the file `offset` rows away from the selected one in the
    /// listing, as the up and down keys do.
    pub(crate) fn review_select_offset(&mut self, offset: isize, cx: &mut Context<Self>) {
        let Some(listing) = self.review.listing().cloned() else {
            return;
        };
        if listing.entries.is_empty() {
            return;
        }
        let current = self
            .review
            .selected()
            .and_then(|path| listing.entries.iter().position(|entry| entry.path == path));
        let last = listing.entries.len() - 1;
        let index = match current {
            None if offset < 0 => last,
            None => 0,
            Some(index) => index.saturating_add_signed(offset).min(last),
        };
        self.review
            .select(Some(listing.entries[index].path.clone()));
        self.review
            .file_scroll
            .scroll_to_item(index, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    pub(crate) fn review_select(
        &mut self,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review.select(Some(path.to_owned()));
        window.focus(&self.review.focus, cx);
        cx.notify();
    }

    /// Keys while the panel, not its comment field, has focus.
    pub(crate) fn review_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        // The comment field's keys are its own; the composer handles Enter and
        // Escape there and the rest is typing.
        if self.review.input.read(cx).focus.is_focused(window) {
            return;
        }
        match keystroke.key.as_str() {
            "up" => self.review_select_offset(-1, cx),
            "down" => self.review_select_offset(1, cx),
            "enter" => {
                if let Some(path) = self.review.selected().map(str::to_owned) {
                    self.review.select(Some(path));
                    cx.notify();
                }
            }
            "space" => {
                if let Some(path) = self.review.selected().map(str::to_owned) {
                    self.review_toggle_staged(&path, cx);
                }
            }
            "escape" => {
                if self.review.draft().is_some() || self.review.picker_open {
                    self.review.cancel_draft();
                    self.review.picker_open = false;
                } else {
                    window.focus(&self.focus, cx);
                }
                cx.notify();
            }
            _ => return,
        }
        cx.stop_propagation();
        window.prevent_default();
    }

    /// Stages an unstaged or partially staged file, unstages a staged one.
    pub(crate) fn review_toggle_staged(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.review.writing() {
            return;
        }
        let Some(entry) = self
            .review
            .listing()
            .and_then(|listing| listing.entry(path).cloned())
        else {
            return;
        };
        let stage = entry.staged() != super::status::Staged::All;
        self.review.stage(path, stage);
        cx.notify();
    }

    /// Stages or unstages every file of one section.
    pub(crate) fn review_stage_section(
        &mut self,
        section: super::status::Section,
        stage: bool,
        cx: &mut Context<Self>,
    ) {
        if self.review.writing() {
            return;
        }
        let paths: Vec<String> = self
            .review
            .listing()
            .map(|listing| {
                listing
                    .entries
                    .iter()
                    .filter(|entry| entry.section() == section)
                    .map(|entry| entry.path.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.review.stage_paths(paths, stage);
        cx.notify();
    }

    pub(crate) fn review_stage_all(&mut self, stage: bool, cx: &mut Context<Self>) {
        if self.review.writing() {
            return;
        }
        self.review.stage_all(stage);
        cx.notify();
    }

    /// Starts or extends a comment on `row` of the loaded diff and puts the
    /// keyboard in the comment field.
    pub(crate) fn review_begin_draft(
        &mut self,
        row: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .review
            .notebook()
            .is_some_and(super::notes::Notebook::is_full)
            && !extend
        {
            self.show_flash(
                Flash::warning(crate::Error::ReviewNotesFull.to_string()),
                cx,
            );
            return;
        }
        let was_drafting = self.review.draft().is_some();
        if !self.review.begin_draft(row, extend) {
            return;
        }
        if !was_drafting || !extend {
            self.review.input.update(cx, |input, cx| input.clear(cx));
        }
        let focus = self.review.input.read(cx).focus.clone();
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Saves the comment field as the draft's note.
    pub(crate) fn review_add_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.review.input.read(cx).text().to_owned();
        match self.review.commit_draft(&text) {
            Ok(()) => {
                self.review.input.update(cx, |input, cx| input.clear(cx));
                window.focus(&self.review.focus, cx);
            }
            Err(error) => self.show_flash(Flash::warning(error.to_string()), cx),
        }
        cx.notify();
    }

    pub(crate) fn review_edit_note(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(comment) = self.review.edit_note(id) else {
            self.show_flash(
                Flash::warning("Open the file this comment is on to edit it"),
                cx,
            );
            return;
        };
        self.review
            .input
            .update(cx, |input, cx| input.set_text_selected(&comment, cx));
        let focus = self.review.input.read(cx).focus.clone();
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn review_cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review.cancel_draft();
        window.focus(&self.review.focus, cx);
        cx.notify();
    }

    pub(crate) fn review_remove_note(&mut self, id: u64, cx: &mut Context<Self>) {
        self.review.remove_note(id);
        cx.notify();
    }

    /// Jumps to the file a note is on.
    pub(crate) fn review_show_note(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(path) = self
            .review
            .notebook()
            .and_then(|book| book.get(id))
            .map(|note| note.anchor.path.clone())
        else {
            return;
        };
        if self.review.selected() != Some(path.as_str()) {
            self.review.select(Some(path));
        }
        cx.notify();
    }

    /// The agents the review can go to, the reviewed workspace's first.
    pub(crate) fn review_candidates(&self) -> Vec<Candidate> {
        let Some(snapshot) = self.live.snapshot.as_deref() else {
            return Vec::new();
        };
        agents::candidates(snapshot, snapshot.focused_workspace_id.as_deref())
    }

    /// The pane a Send without a pick goes to: the user's choice while that
    /// agent is still open, else the default candidate.
    pub(crate) fn review_target(&self) -> Option<Candidate> {
        let candidates = self.review_candidates();
        self.review
            .chosen_pane
            .as_deref()
            .and_then(|pane| {
                candidates
                    .iter()
                    .find(|candidate| candidate.pane_id == pane)
            })
            .or_else(|| agents::default_choice(&candidates))
            .cloned()
    }

    /// The comments as the prompt an agent receives, or `None` without any.
    pub(crate) fn review_prompt(&self) -> Option<String> {
        let book = self.review.notebook().filter(|book| !book.is_empty())?;
        let checkout = self.review.checkout()?;
        let listing = self.review.listing();
        let root = listing
            .map(|listing| listing.root.to_string_lossy().into_owned())
            .or_else(|| checkout.checkout.clone())
            .unwrap_or_else(|| checkout.repo_key.clone());
        let base = listing
            .and_then(|listing| listing.base.as_ref())
            .map(|base| base.name.as_str());
        Some(prompt(
            &Subject {
                root: &root,
                branch: &checkout.branch,
                mode: self.review.mode(),
                base,
            },
            book.notes(),
        ))
    }

    pub(crate) fn copy_review(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.review_prompt() else {
            self.show_flash(Flash::warning("No review comments to copy"), cx);
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.show_flash(Flash::success("Review copied"), cx);
    }

    pub(crate) fn clear_review(&mut self, cx: &mut Context<Self>) {
        self.review.clear_notes();
        self.review.picker_open = false;
        cx.notify();
    }

    /// Sends the comments to the agent in `pane_id`, or to the default one.
    /// They are handed off at once, as the browser's notes are: queued for the
    /// agent's pane, or on the clipboard when no agent can take them, and
    /// cleared here either way so a second Send cannot repeat them.
    pub(crate) fn send_review(&mut self, pane_id: Option<String>, cx: &mut Context<Self>) {
        let Some(text) = self.review_prompt() else {
            self.show_flash(Flash::warning("No review comments to send"), cx);
            return;
        };
        self.review.picker_open = false;
        let target = pane_id.or_else(|| self.review_target().map(|candidate| candidate.pane_id));
        let Some(pane) = target else {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.review.clear_notes();
            self.show_flash(
                Flash::warning("No agent is open, so the review was copied"),
                cx,
            );
            cx.notify();
            return;
        };
        let readiness = self.pane_readiness(&pane);
        let Some(boot_id) = self
            .live
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.boot_id.clone())
            .filter(|_| readiness.accepts())
        else {
            // A shell, not an agent: Enter there would run the review.
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.review.clear_notes();
            self.show_flash(
                Flash::warning("No agent runs in that pane, so the review was copied"),
                cx,
            );
            cx.notify();
            return;
        };
        let flash = if readiness.busy() {
            "Review will go to the agent once it is idle"
        } else {
            "Review sent to the agent"
        };
        self.queue_delivery(
            Delivery::new(
                pane,
                boot_id,
                text,
                Fallback::Clipboard,
                "review",
                Instant::now(),
            ),
            cx,
        );
        self.review.clear_notes();
        self.show_flash(Flash::success(flash), cx);
        cx.notify();
    }

    /// Opens a listed file in the system's default application.
    pub(crate) fn review_open_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let target = self
            .review
            .listing()
            .and_then(|listing| open_target(listing, path));
        match target {
            Some(full) => cx.open_with_system(&full),
            None => self.show_flash(Flash::warning("This file is not on disk to open"), cx),
        }
    }

    /// Shows a listed file in the system's file manager.
    pub(crate) fn review_reveal_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let target = self
            .review
            .listing()
            .and_then(|listing| open_target(listing, path));
        match target {
            Some(full) => cx.reveal_path(&full),
            None => self.show_flash(Flash::warning("This file is not on disk to reveal"), cx),
        }
    }
}
