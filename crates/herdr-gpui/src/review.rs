//! The review panel: a dock beside the terminal that lists the focused
//! checkout's changed files, shows one file's diff, takes comments on its
//! lines, and sends them to an agent running in a pane. Modeled on Zed's git
//! panel and project diff, drawn with plain elements since this client has no
//! editor.
//!
//! Git runs on the panel's own worker thread; the UI thread queues a job and
//! reads the answer on a later tick. A result belongs to the checkout, mode,
//! and file it was asked for, so nothing stale can paint over the current
//! view. The panel is read-only on remote workspaces: only a verified local
//! checkout is listed.
mod agents;
mod diff;
mod notes;
mod panel;
mod status;
mod view;
mod worker;

#[cfg(test)]
mod tests;

use crate::pull_request::Input;
use diff::FileDiff;
use gpui::{App, AppContext as _, Entity, FocusHandle, UniformListScrollHandle};
use notes::{Anchor, Notebook, Notebooks};
use std::{
    collections::VecDeque,
    ops::RangeInclusive,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use worker::{Completion, Job, Listing, Reply, Request};

/// The listing refreshes at the sidebar's cadence while the panel shows and
/// the window is active; a failure waits longer before trying again.
const REFRESH: Duration = Duration::from_secs(5);
const ERROR_BACKOFF: Duration = Duration::from_secs(60);
/// Jobs waiting for the worker. Each is one user gesture or one refresh, and
/// a repeat of a queued job is dropped, so the bound is rarely approached.
const QUEUE_LIMIT: usize = 8;
pub(crate) const DEFAULT_WIDTH: f32 = 440.;
pub(crate) const MIN_WIDTH: f32 = 300.;
/// The panel leaves at least this much of the window to the terminal.
pub(crate) const MIN_CONTENT_WIDTH: f32 = 320.;

/// What the working tree is compared with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The working tree against HEAD: what a commit would take.
    #[default]
    Uncommitted,
    /// The working tree against the merge base with the default branch: what
    /// a pull request would show.
    Branch,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Uncommitted => "Uncommitted",
            Self::Branch => "Branch",
        }
    }
}

/// Where the panel finds the checkout it reviews.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The focused workspace's Herdr worktree, the one the Git popup acts on.
    Worktree(Input),
    /// The directory of the focused tab's pane, in a workspace Herdr keeps no
    /// worktree for. The worker asks Git which checkout holds it.
    Directory(String),
}

/// A comment being written: the diff rows it will cover, in the file whose
/// diff is loaded, and the note it edits when it is not new.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Draft {
    pub path: String,
    /// The row the drag or click started on; the other end moves.
    pub anchor_row: usize,
    pub rows: RangeInclusive<usize>,
    pub editing: Option<u64>,
}

/// One file's loaded diff and where its notes sit in it now.
pub(crate) struct Loaded {
    pub path: String,
    pub diff: Arc<FileDiff>,
    /// Each note on the file and the rows it covers, or `None` once stale.
    pub anchors: Vec<(u64, Option<RangeInclusive<usize>>)>,
}

struct Worker {
    requests: mpsc::SyncSender<(u64, Request)>,
    results: mpsc::Receiver<Reply>,
}

/// The panel's state: what it shows, what it is asking Git, and the notes.
pub(crate) struct Review {
    pub(crate) open: bool,
    pub(crate) width: f32,
    /// A drag of the panel's edge: where it started and the width then.
    pub(crate) drag: Option<(f32, f32)>,
    /// The panel's keyboard focus, separate from the terminal's.
    pub(crate) focus: FocusHandle,
    pub(crate) input: Entity<crate::search_input::SearchInput>,
    pub(crate) file_scroll: UniformListScrollHandle,
    pub(crate) diff_scroll: UniformListScrollHandle,
    mode: Mode,
    worker: Option<Worker>,
    generation: Arc<AtomicU64>,
    busy: Option<Job>,
    queue: VecDeque<Job>,
    source: Option<Source>,
    /// A directory source asks Git for its checkout before the next listing:
    /// the branch checked out there can change while the directory does not.
    resolve_due: bool,
    /// A directory's checkout is being looked up.
    resolving: bool,
    checkout: Option<Input>,
    listing: Option<Arc<Listing>>,
    selected: Option<String>,
    loaded: Option<Loaded>,
    /// The file whose diff was asked for and has not arrived.
    loading: Option<String>,
    due: Option<Instant>,
    error: Option<String>,
    notebooks: Notebooks,
    draft: Option<Draft>,
    /// A drag over diff rows extends the draft while the button is down.
    pub(crate) dragging: bool,
    pub(crate) picker_open: bool,
    /// The pane chosen in the agent picker, when the user picked one.
    pub(crate) chosen_pane: Option<String>,
    pub(crate) notes_expanded: bool,
    /// Whether a titlebar Git action was running last tick: its end means
    /// the tree changed.
    git_was_running: bool,
}

impl Drop for Review {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

impl Review {
    pub(crate) fn new(cx: &mut App) -> Self {
        let input = cx.new(|cx| {
            let mut input = crate::search_input::SearchInput::new(cx);
            input.set_placeholder("Add a review comment\u{2026}", cx);
            input
        });
        Self {
            open: false,
            width: DEFAULT_WIDTH,
            drag: None,
            focus: cx.focus_handle(),
            input,
            file_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            mode: Mode::default(),
            worker: None,
            generation: Arc::default(),
            busy: None,
            queue: VecDeque::new(),
            source: None,
            resolve_due: false,
            resolving: false,
            checkout: None,
            listing: None,
            selected: None,
            loaded: None,
            loading: None,
            due: None,
            error: None,
            notebooks: Notebooks::default(),
            draft: None,
            dragging: false,
            picker_open: false,
            chosen_pane: None,
            notes_expanded: false,
            git_was_running: false,
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn checkout(&self) -> Option<&Input> {
        self.checkout.as_ref()
    }

    pub(crate) fn listing(&self) -> Option<&Arc<Listing>> {
        self.listing.as_ref()
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub(crate) fn loaded(&self) -> Option<&Loaded> {
        self.loaded.as_ref()
    }

    pub(crate) fn loading(&self) -> Option<&str> {
        self.loading.as_deref()
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn draft(&self) -> Option<&Draft> {
        self.draft.as_ref()
    }

    /// Whether a staging write is in flight or queued, during which the
    /// checkboxes do not take another.
    pub(crate) fn writing(&self) -> bool {
        self.busy.as_ref().is_some_and(Job::is_write) || self.queue.iter().any(Job::is_write)
    }

    /// Git is being asked which checkout holds the focused tab's directory
    /// for the first time. A later check of a directory outside any checkout
    /// keeps showing Git's last answer rather than flickering.
    pub(crate) fn finding(&self) -> bool {
        self.checkout.is_none() && self.error.is_none() && (self.resolving || self.resolve_due)
    }

    pub(crate) fn refreshing(&self) -> bool {
        self.resolving
            || matches!(self.busy, Some(Job::List(_)))
            || self.queue.iter().any(|job| matches!(job, Job::List(_)))
    }

    /// The notes of the tracked checkout.
    pub(crate) fn notebook(&self) -> Option<&Notebook> {
        let checkout = self.checkout.as_ref()?;
        self.notebooks.get(&checkout.repo_key, &checkout.branch)
    }

    pub(crate) fn notebook_mut(&mut self) -> Option<&mut Notebook> {
        let checkout = self.checkout.clone()?;
        Some(self.notebooks.get_mut(&checkout.repo_key, &checkout.branch))
    }

    pub(crate) fn note_count(&self) -> usize {
        self.notebook().map_or(0, Notebook::len)
    }

    /// Follows the focused tab's checkout and schedules the listing's
    /// refresh. `refresh` is false while the panel is hidden or the window
    /// inactive: what is on screen stays, and Git is not polled behind the
    /// user's back, not even to find a directory's checkout.
    pub(crate) fn track(&mut self, source: Option<Source>, refresh: bool, now: Instant) -> bool {
        let mut changed = false;
        if self.source != source {
            self.source = source;
            self.resolve_due = false;
            self.error = None;
            changed = true;
            match self.source.clone() {
                Some(Source::Worktree(input)) => {
                    self.set_checkout(Some(input), now);
                }
                // The checkout shown stays until Git says which one holds the
                // directory, so moving within a checkout does not blank it.
                Some(Source::Directory(_)) => self.due = Some(now),
                None => {
                    self.set_checkout(None, now);
                }
            }
        }
        if refresh
            && self.source.is_some()
            && self.due.is_some_and(|due| now >= due)
            && !self.refreshing()
        {
            self.due = None;
            self.resolve_due = self.resolves();
            if self.checkout.is_some() {
                self.enqueue(Job::List(self.mode));
            }
        }
        changed
    }

    /// Shows `checkout`, dropping everything drawn from the one shown before.
    /// Notes stay in their checkout's notebook.
    fn set_checkout(&mut self, checkout: Option<Input>, now: Instant) -> bool {
        if self.checkout == checkout {
            return false;
        }
        self.checkout = checkout;
        self.listing = None;
        self.selected = None;
        self.loaded = None;
        self.loading = None;
        self.draft = None;
        self.error = None;
        self.queue.clear();
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.due = Some(now);
        true
    }

    fn resolves(&self) -> bool {
        matches!(self.source, Some(Source::Directory(_)))
    }

    /// Asks for the listing again now.
    pub(crate) fn refresh(&mut self) {
        if self.refreshing() {
            return;
        }
        self.resolve_due = self.resolves();
        if self.checkout.is_some() {
            self.due = None;
            self.enqueue(Job::List(self.mode));
        }
    }

    pub(crate) fn set_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.listing = None;
        self.loaded = None;
        self.loading = None;
        self.draft = None;
        self.error = None;
        self.queue.retain(|job| job.is_write());
        self.refresh();
    }

    fn enqueue(&mut self, job: Job) {
        if self.busy.as_ref() == Some(&job) || self.queue.contains(&job) {
            return;
        }
        if self.queue.len() >= QUEUE_LIMIT {
            self.queue.pop_front();
        }
        self.queue.push_back(job);
    }

    /// Selects `path` and asks for its diff; selecting the shown file again
    /// reads its diff afresh.
    pub(crate) fn select(&mut self, path: Option<String>) {
        if self.selected != path {
            self.draft = None;
        }
        self.selected = path;
        self.request_diff();
    }

    fn request_diff(&mut self) {
        let (Some(listing), Some(path)) = (&self.listing, &self.selected) else {
            return;
        };
        let Some(entry) = listing.entry(path) else {
            return;
        };
        let job = Job::Diff {
            mode: listing.mode,
            path: path.clone(),
            compare: listing.compare(entry),
        };
        self.loading = Some(path.clone());
        self.enqueue(job);
    }

    /// The staging job for one path, if the listing can stage it.
    pub(crate) fn stage(&mut self, path: &str, stage: bool) {
        let Some(listing) = &self.listing else {
            return;
        };
        if listing.mode != Mode::Uncommitted || listing.entry(path).is_none() {
            return;
        }
        let unborn = listing.unborn();
        let paths = vec![path.to_owned()];
        self.enqueue(if stage {
            Job::Stage(paths)
        } else {
            Job::Unstage { paths, unborn }
        });
    }

    pub(crate) fn stage_paths(&mut self, paths: Vec<String>, stage: bool) {
        let Some(listing) = &self.listing else {
            return;
        };
        if listing.mode != Mode::Uncommitted || paths.is_empty() {
            return;
        }
        let unborn = listing.unborn();
        self.enqueue(if stage {
            Job::Stage(paths)
        } else {
            Job::Unstage { paths, unborn }
        });
    }

    pub(crate) fn stage_all(&mut self, stage: bool) {
        let Some(listing) = &self.listing else {
            return;
        };
        if listing.mode != Mode::Uncommitted {
            return;
        }
        let unborn = listing.unborn();
        self.enqueue(if stage {
            Job::StageAll
        } else {
            Job::UnstageAll { unborn }
        });
    }

    /// Starts a comment on `row` of the loaded diff, or moves the far end of
    /// the draft there when `extend`.
    pub(crate) fn begin_draft(&mut self, row: usize, extend: bool) -> bool {
        let Some(loaded) = &self.loaded else {
            return false;
        };
        if !loaded.diff.rows.get(row).is_some_and(diff::Row::is_line) {
            return false;
        }
        match &mut self.draft {
            Some(draft) if extend && draft.path == loaded.path => {
                let anchor = draft.anchor_row;
                draft.rows = clamp_range(&loaded.diff, anchor, row);
            }
            _ => {
                self.draft = Some(Draft {
                    path: loaded.path.clone(),
                    anchor_row: row,
                    rows: row..=row,
                    editing: None,
                });
            }
        }
        true
    }

    pub(crate) fn cancel_draft(&mut self) {
        self.draft = None;
        self.dragging = false;
    }

    /// Opens the draft on an existing note's rows, to change its comment.
    pub(crate) fn edit_note(&mut self, id: u64) -> Option<String> {
        let comment = self.notebook()?.get(id)?.comment.clone();
        let loaded = self.loaded.as_ref()?;
        let rows = loaded
            .anchors
            .iter()
            .find(|(note, _)| *note == id)
            .and_then(|(_, rows)| rows.clone())?;
        self.draft = Some(Draft {
            path: loaded.path.clone(),
            anchor_row: *rows.start(),
            rows,
            editing: Some(id),
        });
        Some(comment)
    }

    /// Saves the draft as a note with `comment`, or updates the note it edits.
    pub(crate) fn commit_draft(&mut self, comment: &str) -> crate::Result<()> {
        let draft = self.draft.clone().ok_or(crate::Error::ReviewEmptyComment)?;
        // Checked here too, so a refused comment does not use up a note id.
        if notes::single_line(comment, notes::MAX_COMMENT_CHARS).is_empty() {
            return Err(crate::Error::ReviewEmptyComment);
        }
        let loaded = self
            .loaded
            .as_ref()
            .filter(|loaded| loaded.path == draft.path);
        let mode = self.mode;
        if let Some(id) = draft.editing {
            self.notebook_mut()
                .ok_or(crate::Error::GitNoCheckout)?
                .edit(id, comment)?;
        } else {
            let loaded = loaded.ok_or(crate::Error::ReviewEmptyComment)?;
            let anchor = Anchor::capture(&draft.path, &loaded.diff, draft.rows.clone())
                .ok_or(crate::Error::ReviewEmptyComment)?;
            let id = self.notebooks.next_id();
            self.notebook_mut()
                .ok_or(crate::Error::GitNoCheckout)?
                .add(id, anchor, comment, mode)?;
        }
        self.draft = None;
        self.dragging = false;
        self.place_notes();
        Ok(())
    }

    pub(crate) fn remove_note(&mut self, id: u64) {
        if let Some(book) = self.notebook_mut() {
            book.remove(id);
        }
        if self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.editing == Some(id))
        {
            self.draft = None;
        }
        self.place_notes();
    }

    pub(crate) fn clear_notes(&mut self) {
        if let Some(book) = self.notebook_mut() {
            book.clear();
        }
        self.draft = None;
        self.place_notes();
    }

    /// Finds each note of the loaded file in its diff.
    fn place_notes(&mut self) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        let checkout = self.checkout.as_ref();
        let notes = checkout
            .and_then(|checkout| self.notebooks.get(&checkout.repo_key, &checkout.branch))
            .map(|book| {
                book.on_path(&loaded.path)
                    .map(|note| (note.id, note.anchor.locate(&loaded.diff)))
                    .collect()
            })
            .unwrap_or_default();
        loaded.anchors = notes;
    }

    /// Whether a note is stale: written on lines the current diff no longer
    /// has. Unknown, and so not stale, for a file whose diff is not loaded.
    pub(crate) fn is_stale(&self, id: u64) -> bool {
        self.loaded.as_ref().is_some_and(|loaded| {
            loaded
                .anchors
                .iter()
                .any(|(note, rows)| *note == id && rows.is_none())
        })
    }

    /// Drains the worker's answers and sends it the next job. `true` when the
    /// panel has to repaint.
    pub(crate) fn poll(&mut self, git_running: bool, now: Instant) -> bool {
        let mut changed = false;
        // A commit or push from the titlebar changed the tree it reported on.
        if self.git_was_running && !git_running {
            self.due = Some(now);
        }
        self.git_was_running = git_running;
        if let Some(worker) = &self.worker {
            match worker.results.try_recv() {
                Ok(Reply::Resolved { directory, result }) => {
                    self.resolving = false;
                    changed |= self.resolved(&directory, result, now);
                }
                Ok(Reply::Ran(input, completion)) => {
                    self.busy = None;
                    changed = true;
                    if self.checkout.as_ref() == Some(&input) {
                        self.apply(completion, now);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.busy = None;
                    self.resolving = false;
                    self.queue.clear();
                    self.worker = None;
                    self.loading = None;
                    self.error = Some(crate::Error::ReviewWorker.to_string());
                    self.due = Some(now + ERROR_BACKOFF);
                    return true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.busy.is_some() || self.resolving {
            return changed;
        }
        // The checkout is found before any job runs against it.
        if self.resolve_due
            && let Some(Source::Directory(directory)) = &self.source
        {
            self.resolve_due = false;
            let request = Request::Resolve(directory.clone());
            changed |= self.send(request, now);
        } else if let Some(checkout) = self.checkout.clone()
            && let Some(job) = self.queue.pop_front()
        {
            changed |= self.send(Request::Run(checkout, job), now);
        }
        changed
    }

    /// Shows the checkout Git found for `directory`, while it is still the
    /// focused tab's. A directory outside any checkout, or on a detached
    /// HEAD, has nothing to review; Git's reason is kept for the panel.
    fn resolved(&mut self, directory: &str, result: crate::Result<Input>, now: Instant) -> bool {
        if !matches!(&self.source, Some(Source::Directory(current)) if current == directory) {
            return false;
        }
        match result {
            Ok(input) => {
                if !self.set_checkout(Some(input), now) {
                    return false;
                }
                // Found while refreshing, so the listing follows at once.
                self.due = None;
                self.enqueue(Job::List(self.mode));
                true
            }
            Err(error) => {
                self.set_checkout(None, now);
                self.error = Some(error.to_string());
                self.due = Some(now + REFRESH);
                true
            }
        }
    }

    fn apply(&mut self, completion: Completion, now: Instant) {
        match completion {
            Completion::List(Ok(listing)) if listing.mode == self.mode => {
                self.error = None;
                self.due = Some(now + REFRESH);
                let previous = self.listing.replace(Arc::new(listing));
                let listing = self.listing.clone();
                let Some(listing) = listing else {
                    return;
                };
                // The selection follows the file, not its row; a file that
                // left the listing leaves the diff view too.
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|path| listing.entry(path).is_none())
                {
                    self.selected = None;
                    self.loaded = None;
                    self.loading = None;
                    self.draft = None;
                }
                let same = previous.as_deref().is_some_and(|previous| {
                    previous.entries == listing.entries
                        && previous.additions == listing.additions
                        && previous.deletions == listing.deletions
                        && previous.head == listing.head
                });
                if !same {
                    self.request_diff();
                }
            }
            Completion::List(Ok(_)) => {}
            Completion::List(Err(error)) => {
                self.error = Some(error.to_string());
                self.due = Some(now + ERROR_BACKOFF);
            }
            Completion::Diff { mode, path, result } => {
                if mode != self.mode || self.selected.as_deref() != Some(path.as_str()) {
                    return;
                }
                if self.loading.as_deref() == Some(path.as_str()) {
                    self.loading = None;
                }
                match result {
                    Ok(diff) => {
                        // The draft's rows meant the old diff.
                        if self
                            .draft
                            .as_ref()
                            .is_some_and(|draft| draft.editing.is_none())
                            && self
                                .loaded
                                .as_ref()
                                .is_some_and(|loaded| loaded.diff.rows != diff.rows)
                        {
                            self.draft = None;
                        }
                        self.loaded = Some(Loaded {
                            path,
                            diff: Arc::new(diff),
                            anchors: Vec::new(),
                        });
                        self.place_notes();
                        self.error = None;
                    }
                    Err(error) => {
                        self.loaded = None;
                        self.error = Some(error.to_string());
                    }
                }
            }
            Completion::Write(result) => {
                if let Err(error) = result {
                    self.error = Some(error.to_string());
                }
                // Whatever the write did, the tree is worth reading again.
                self.due = None;
                self.enqueue(Job::List(self.mode));
            }
        }
    }

    fn send(&mut self, request: Request, now: Instant) -> bool {
        if self.worker.is_none() {
            let (requests, incoming) = mpsc::sync_channel::<(u64, Request)>(1);
            let (outgoing, results) = mpsc::sync_channel(1);
            let current = self.generation.clone();
            match thread::Builder::new()
                .name("herdr-review".into())
                .spawn(move || {
                    for (generation, request) in incoming {
                        let cancelled = || current.load(Ordering::Relaxed) != generation;
                        if outgoing.send(worker::answer(request, &cancelled)).is_err() {
                            break;
                        }
                    }
                }) {
                Ok(_) => self.worker = Some(Worker { requests, results }),
                Err(source) => {
                    self.error = Some(
                        crate::Error::GitProcess {
                            operation: "start the review worker",
                            source,
                        }
                        .to_string(),
                    );
                    self.due = Some(now + ERROR_BACKOFF);
                    return true;
                }
            }
        }
        let Some(worker) = &self.worker else {
            return false;
        };
        let generation = self.generation.load(Ordering::Relaxed);
        match worker.requests.try_send((generation, request.clone())) {
            Ok(()) => {
                match request {
                    Request::Resolve(_) => self.resolving = true,
                    Request::Run(_, job) => self.busy = Some(job),
                }
                false
            }
            Err(mpsc::TrySendError::Full(_)) => {
                match request {
                    Request::Resolve(_) => self.resolve_due = true,
                    Request::Run(_, job) => self.queue.push_front(job),
                }
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.worker = None;
                self.error = Some(crate::Error::ReviewWorker.to_string());
                self.due = Some(now + ERROR_BACKOFF);
                true
            }
        }
    }
}

/// The rows from `anchor` to `end`, in order, cut at the hunk header between
/// them and at the note bound.
fn clamp_range(diff: &FileDiff, anchor: usize, end: usize) -> RangeInclusive<usize> {
    let (mut start, mut stop) = (anchor.min(end), anchor.max(end));
    // No note crosses a hunk header: the rows on the other side are another
    // place in the file.
    if let Some(header) = (start..=stop)
        .filter(|row| matches!(diff.rows.get(*row), Some(diff::Row::Hunk(_))))
        .min_by_key(|row| row.abs_diff(anchor))
    {
        if header < anchor {
            start = header + 1;
        } else {
            stop = header.saturating_sub(1).max(start);
        }
    }
    if stop - start + 1 > notes::MAX_NOTE_ROWS {
        if anchor <= start {
            stop = start + notes::MAX_NOTE_ROWS - 1;
        } else {
            start = stop + 1 - notes::MAX_NOTE_ROWS;
        }
    }
    start..=stop
}
