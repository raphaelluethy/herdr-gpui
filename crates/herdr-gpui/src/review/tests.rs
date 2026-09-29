#![allow(clippy::unwrap_used)]
//! Headless tests of the panel in a window: toggling, selecting a file,
//! writing a comment, and sending it. Git runs against a repository made for
//! the test; the daemon is a fixture snapshot.
use super::{Mode, Review, Source, status::Staged, view::open_target, worker::tests::Repo};
use crate::{
    HerdrWindow,
    controls::Command,
    sidebar::layout_tests::{fixture_window, full_draw},
};
use gpui::{Entity, TestAppContext, VisualTestContext, px, size};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn window(cx: &mut TestAppContext) -> (Entity<HerdrWindow>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|window, cx| {
        crate::bind_keys(cx);
        fixture_window(window, cx)
    });
    cx.simulate_resize(size(px(1400.), px(800.)));
    (view, cx)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| full_draw(window, cx).clear(cx));
}

/// Drains the worker until `ready` holds, within a bound: the worker is a
/// real thread running real Git, so the wait is on it, not on a guess.
fn wait_for(
    view: &Entity<HerdrWindow>,
    cx: &mut VisualTestContext,
    what: &str,
    ready: impl Fn(&Review) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let done = cx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.review.poll(false, Instant::now());
                ready(&view.review)
            })
        });
        if done {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Opens the panel on `repo` and waits for its first listing.
fn open_on(repo: &Repo, view: &Entity<HerdrWindow>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.toggle_review(window, cx);
            view.review.track(
                Some(Source::Worktree(repo.input.clone())),
                true,
                Instant::now(),
            );
        })
    });
    wait_for(view, cx, "the listing", |review| review.listing().is_some());
}

fn select(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext, path: &str) {
    cx.update(|window, cx| view.update(cx, |view, cx| view.review_select(path, window, cx)));
    wait_for(view, cx, "the diff", |review| {
        review.loaded().is_some_and(|loaded| loaded.path == path)
    });
}

fn add_note(
    view: &Entity<HerdrWindow>,
    cx: &mut VisualTestContext,
    rows: (usize, usize),
    text: &str,
) {
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.review_begin_draft(rows.0, false, window, cx);
            view.review_begin_draft(rows.1, true, window, cx);
            let input = view.review.input.clone();
            input.update(cx, |input, cx| input.set_text_selected(text, cx));
            view.review_add_note(window, cx);
        })
    });
}

/// A snapshot where pane `w0:p1` runs an agent with `status`, or a shell.
fn with_pane(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext, agent: Option<&str>) {
    cx.update(|_, cx| {
        view.update(cx, |view, _| {
            let mut shown: serde_json::Value =
                serde_json::to_value(view.live.snapshot.as_deref().unwrap()).unwrap();
            shown["focused_workspace_id"] = serde_json::json!("w0");
            shown["panes"] = serde_json::json!([{
                "pane_id": "w0:p1", "workspace_id": "w0", "tab_id": "t0", "label": null,
                "cwd": null, "foreground_cwd": null, "focused": true,
                "right_click_passthrough": false
            }]);
            shown["agents"] = serde_json::json!(
                agent
                    .map(|status| {
                        vec![serde_json::json!({
                            "pane_id": "w0:p1", "workspace_id": "w0", "tab_id": "t0",
                            "name": "review", "display_agent": "Claude Code", "agent": "claude",
                            "title": null, "terminal_title": null, "terminal_title_stripped": null,
                            "agent_status": status, "state_change_seq": 0, "state_labels": [],
                            "tokens": [], "focused": true
                        })]
                    })
                    .unwrap_or_default()
            );
            view.live.snapshot = Some(Arc::new(serde_json::from_value(shown).unwrap()));
        });
    });
}

/// The owned local daemon, focused on workspace `w0`, which Herdr keeps no
/// worktree for, with its focused pane working in `directory`.
fn focus_pane_in(view: &Entity<HerdrWindow>, cx: &mut VisualTestContext, directory: &str) {
    cx.update(|_, cx| {
        view.update(cx, |view, _| {
            view.live.status = crate::state::ConnectionStatus::Connected;
            view.live.local_daemon_peer = true;
            view.selected_endpoint = 0;
            view.active = true;
            let mut shown: serde_json::Value =
                serde_json::to_value(view.live.snapshot.as_deref().unwrap()).unwrap();
            shown["focused_workspace_id"] = serde_json::json!("w0");
            shown["focused_pane_id"] = serde_json::json!("w0:p1");
            shown["panes"] = serde_json::json!([{
                "pane_id": "w0:p1", "workspace_id": "w0", "tab_id": "t0", "label": null,
                "cwd": "/", "foreground_cwd": directory, "focused": true,
                "right_click_passthrough": false
            }]);
            view.live.snapshot = Some(Arc::new(serde_json::from_value(shown).unwrap()));
            assert!(view.git.tracked().is_none(), "no Herdr worktree to follow");
        });
    });
}

/// Runs the window's own review tick until `ready` holds, within a bound.
fn follow_until(
    view: &Entity<HerdrWindow>,
    cx: &mut VisualTestContext,
    what: &str,
    ready: impl Fn(&Review) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let done = cx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.update_review();
                ready(&view.review)
            })
        });
        if done {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
}

#[gpui::test]
fn the_panel_toggles_beside_the_terminal_and_explains_when_there_is_no_checkout(
    cx: &mut TestAppContext,
) {
    let (view, cx) = window(cx);
    draw(cx);
    assert!(cx.debug_bounds("review-panel").is_none());
    let terminal_before = cx.debug_bounds("terminal").unwrap();
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.command(Command::ToggleReviewPanel, window, cx);
            assert!(view.review.open);
            assert!(
                view.review.focus.is_focused(window),
                "the panel takes the keyboard"
            );
        })
    });
    draw(cx);
    let panel = cx.debug_bounds("review-panel").unwrap();
    let terminal = cx.debug_bounds("terminal").unwrap();
    assert!(
        terminal.right() <= panel.left(),
        "the panel docks right of the terminal"
    );
    assert!(terminal.size.width < terminal_before.size.width);
    assert!(panel.size.width >= px(super::MIN_WIDTH));
    // Without a local checkout the panel says so instead of listing nothing.
    assert!(cx.debug_bounds("review-unavailable").is_some());
    assert!(cx.debug_bounds("review-files").is_none());
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.command(Command::ToggleReviewPanel, window, cx);
            assert!(!view.review.open);
            assert!(
                view.focus.is_focused(window),
                "closing returns the keyboard"
            );
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("review-panel").is_none());
    assert_eq!(
        cx.debug_bounds("terminal").unwrap().size.width,
        terminal_before.size.width
    );
}

#[gpui::test]
fn a_workspace_without_a_herdr_worktree_is_reviewed_from_its_open_tab(cx: &mut TestAppContext) {
    let repo = Repo::new();
    repo.write("dir/new.txt", "fresh\n");
    let subdirectory = repo.directory.path().join("dir");
    let (view, cx) = window(cx);
    focus_pane_in(&view, cx, subdirectory.to_str().unwrap());
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.update_review();
            view.toggle_review(window, cx);
        })
    });
    follow_until(&view, cx, "the listing", |review| {
        review.listing().is_some()
    });
    let root = repo.git(&["rev-parse", "--show-toplevel"]);
    view.update(cx, |view, _| {
        let checkout = view.review.checkout().unwrap();
        assert_eq!(checkout.checkout.as_deref(), Some(root.as_str()));
        assert_eq!(checkout.branch, "feature");
        assert!(
            view.review
                .listing()
                .unwrap()
                .entry("dir/new.txt")
                .is_some()
        );
    });
    draw(cx);
    assert!(cx.debug_bounds("review-branch").is_some());
    assert!(cx.debug_bounds("review-unavailable").is_none());

    // Moving within the checkout keeps what is shown while Git is asked.
    focus_pane_in(&view, cx, &root);
    view.update(cx, |view, _| {
        view.update_review();
        assert!(view.review.listing().is_some());
    });
    follow_until(&view, cx, "the lookup", |review| !review.refreshing());
    view.update(cx, |view, _| {
        assert_eq!(
            view.review.checkout().unwrap().checkout.as_deref(),
            Some(root.as_str())
        );
        assert!(view.review.listing().is_some());
    });

    // A tab outside any checkout has nothing to review, and says why.
    let outside = tempfile::tempdir().unwrap();
    focus_pane_in(&view, cx, outside.path().to_str().unwrap());
    follow_until(&view, cx, "the lookup", |review| {
        review.checkout().is_none() && !review.finding()
    });
    view.update(cx, |view, _| assert!(view.review.error().is_some()));
    draw(cx);
    assert!(cx.debug_bounds("review-unavailable").is_some());
    assert!(cx.debug_bounds("review-unavailable-reason").is_some());
    assert!(cx.debug_bounds("review-files").is_none());
    // Checking again keeps the answer on screen instead of flickering.
    view.update(cx, |view, _| {
        view.review.refresh();
        assert!(!view.review.finding());
    });
}

#[gpui::test]
fn a_lookup_for_a_tab_no_longer_focused_is_dropped(cx: &mut TestAppContext) {
    let repo = Repo::new();
    let outside = tempfile::tempdir().unwrap();
    let (view, cx) = window(cx);
    let first = Some(Source::Directory(
        repo.directory.path().to_str().unwrap().to_owned(),
    ));
    let second = Some(Source::Directory(
        outside.path().to_str().unwrap().to_owned(),
    ));
    view.update(cx, |view, _| {
        let now = Instant::now();
        view.review.track(first, true, now);
        view.review.poll(false, now);
        assert!(view.review.finding(), "the first lookup is on its way");
        view.review.track(second.clone(), true, now);
    });
    // The first answer arrives after the focus moved and must not be shown.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let done = view.update(cx, |view, _| {
            let now = Instant::now();
            view.review.track(second.clone(), true, now);
            view.review.poll(false, now);
            assert!(view.review.checkout().is_none(), "a stale lookup was shown");
            !view.review.finding() && view.review.error().is_some()
        });
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the lookup"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[gpui::test]
fn the_panel_width_is_kept_between_its_bound_and_the_terminals(cx: &mut TestAppContext) {
    let (view, cx) = window(cx);
    view.update(cx, |view, _| {
        view.review.width = 100.;
        assert_eq!(view.review_width(1400.), super::MIN_WIDTH);
        view.review.width = 2000.;
        let widest = view.review_width(1400.);
        assert!(widest < 1400. - super::MIN_CONTENT_WIDTH);
        assert!(widest >= super::MIN_WIDTH);
        view.review.width = super::DEFAULT_WIDTH;
        assert_eq!(view.review_width(1400.), super::DEFAULT_WIDTH);
        // A tiny window still leaves the panel its minimum.
        assert_eq!(view.review_width(400.), super::MIN_WIDTH);
    });
}

#[gpui::test]
fn files_are_listed_selected_diffed_and_staged(cx: &mut TestAppContext) {
    let repo = Repo::new();
    repo.write("tracked.txt", "one\n2\nthree\n");
    repo.write("dir/new.txt", "fresh\n");
    let (view, cx) = window(cx);
    open_on(&repo, &view, cx);
    draw(cx);
    assert!(cx.debug_bounds("review-branch").is_some());
    assert!(cx.debug_bounds("review-section-Tracked").is_some());
    assert!(cx.debug_bounds("review-section-Untracked").is_some());
    let first = cx.debug_bounds("review-file-0").unwrap();
    let second = cx.debug_bounds("review-file-1").unwrap();
    assert!(first.bottom() <= second.top());
    view.read_with(cx, |view, _| {
        let listing = view.review.listing().unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert_eq!(view.review.file_row_count(), 4);
        assert!(view.review.selected().is_none());
    });
    // Down selects the first file and loads its diff.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            assert!(view.review.focus.is_focused(window));
            view.review_select_offset(1, cx);
        })
    });
    wait_for(&view, cx, "the first diff", |review| {
        review.loaded().is_some()
    });
    view.read_with(cx, |view, _| {
        assert_eq!(view.review.selected(), Some("tracked.txt"));
        assert_eq!(view.review.loaded().unwrap().diff.rows.len(), 5);
    });
    draw(cx);
    assert!(cx.debug_bounds("review-diff-header").is_some());
    assert!(
        cx.debug_bounds("review-line-0").is_some(),
        "the hunk header"
    );
    assert!(cx.debug_bounds("review-line-4").is_some());
    assert!(cx.debug_bounds("review-open").is_some());
    // The untracked file shows as all added.
    select(&view, cx, "dir/new.txt");
    view.read_with(cx, |view, _| {
        let loaded = view.review.loaded().unwrap();
        assert_eq!(loaded.diff.additions, 1);
        assert!(!loaded.diff.binary);
    });
    // Space stages the selected file; the listing refreshes on its own.
    cx.update(|_, cx| view.update(cx, |view, cx| view.review_toggle_staged("dir/new.txt", cx)));
    wait_for(&view, cx, "the file to be staged", |review| {
        review
            .listing()
            .and_then(|listing| listing.entry("dir/new.txt"))
            .is_some_and(|entry| entry.staged() == Staged::All)
    });
    cx.update(|_, cx| view.update(cx, |view, cx| view.review_stage_all(false, cx)));
    wait_for(&view, cx, "everything to be unstaged", |review| {
        review.listing().is_some_and(|listing| {
            listing
                .entries
                .iter()
                .all(|entry| entry.staged() == Staged::None)
        }) && !review.writing()
    });
    // A file that is not on disk cannot be opened; the others open under the
    // verified checkout.
    std::fs::remove_file(repo.directory.path().join("keep.txt")).unwrap();
    cx.update(|_, cx| view.update(cx, |view, _| view.review.refresh()));
    wait_for(&view, cx, "the deletion", |review| {
        review
            .listing()
            .is_some_and(|listing| listing.entry("keep.txt").is_some())
    });
    view.read_with(cx, |view, _| {
        let listing = view.review.listing().unwrap();
        assert!(open_target(listing, "keep.txt").is_none());
        assert!(open_target(listing, "../escape").is_none());
        assert_eq!(
            open_target(listing, "dir/new.txt").unwrap(),
            listing.root.join("dir/new.txt")
        );
    });
    // Branch mode compares with the default branch.
    cx.update(|_, cx| view.update(cx, |view, cx| view.set_review_mode(Mode::Branch, cx)));
    wait_for(&view, cx, "the branch listing", |review| {
        review
            .listing()
            .is_some_and(|listing| listing.mode == Mode::Branch)
    });
    draw(cx);
    assert!(cx.debug_bounds("review-base").is_some());
    wait_for(&view, cx, "the branch diff", |review| {
        review
            .loaded()
            .is_some_and(|loaded| loaded.path == "dir/new.txt")
    });
    view.read_with(cx, |view, _| {
        let listing = view.review.listing().unwrap();
        assert_eq!(listing.base.as_ref().unwrap().name, "main");
        assert_eq!(
            view.review.selected(),
            Some("dir/new.txt"),
            "the file stays selected across modes"
        );
    });
}

#[gpui::test]
fn comments_are_written_inline_kept_per_checkout_and_go_stale(cx: &mut TestAppContext) {
    let repo = Repo::new();
    repo.write("tracked.txt", "one\n2\nthree\n");
    let (view, cx) = window(cx);
    open_on(&repo, &view, cx);
    select(&view, cx, "tracked.txt");
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            // Rows 2 and 3: the removed and the added line.
            view.review_begin_draft(2, false, window, cx);
            view.review_begin_draft(3, true, window, cx);
            let draft = view.review.draft().unwrap();
            assert_eq!(draft.rows, 2..=3);
            assert!(view.review.input.read(cx).focus.is_focused(window));
            // A hunk header cannot be covered.
            view.review_begin_draft(0, true, window, cx);
            assert_eq!(view.review.draft().unwrap().rows, 2..=3);
        })
    });
    draw(cx);
    let draft = cx.debug_bounds("review-draft").unwrap();
    let covered = cx.debug_bounds("review-line-3").unwrap();
    assert!(
        covered.bottom() <= draft.top(),
        "the composer sits under the range"
    );
    // Arrow keys in the comment field are typing, not file navigation.
    cx.simulate_keystrokes("down");
    view.read_with(cx, |view, _| {
        assert_eq!(view.review.selected(), Some("tracked.txt"));
        assert!(view.review.draft().is_some());
    });
    // An empty comment is refused and the draft stays.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.review_add_note(window, cx);
            assert!(view.review.draft().is_some());
            assert_eq!(view.review.note_count(), 0);
            let input = view.review.input.clone();
            input.update(cx, |input, cx| input.set_text_selected("Use a word", cx));
            view.review_add_note(window, cx);
            assert!(view.review.draft().is_none());
            assert_eq!(view.review.note_count(), 1);
            assert!(view.review.focus.is_focused(window));
        })
    });
    draw(cx);
    let note_row = cx.debug_bounds("review-note-1");
    assert!(note_row.is_some(), "the note shows inline");
    let note_row = note_row.unwrap();
    let line = cx.debug_bounds("review-line-4").unwrap();
    let header = cx.debug_bounds("review-diff-header").unwrap();
    assert!(note_row.top() >= cx.debug_bounds("review-line-3").unwrap().bottom());
    assert!(
        note_row.bottom() <= line.top(),
        "under the last covered line"
    );
    assert_eq!(line.size.width, header.size.width, "rows span the panel");
    assert!(cx.debug_bounds("review-send").is_some());
    let (id, anchor) = view.read_with(cx, |view, _| {
        let note = &view.review.notebook().unwrap().notes()[0];
        (note.id, note.anchor.clone())
    });
    assert_eq!(anchor.path, "tracked.txt");
    assert_eq!(anchor.lines, 2..=2);
    assert_eq!(anchor.snippet, ["-two", "+2"]);
    // Editing keeps the anchor and changes the comment.
    cx.update(|window, cx| {
        view.update(cx, |view, cx| {
            view.review_edit_note(id, window, cx);
            assert_eq!(view.review.input.read(cx).text(), "Use a word");
            let input = view.review.input.clone();
            input.update(cx, |input, cx| input.set_text_selected("Spell it out", cx));
            view.review_add_note(window, cx);
            assert_eq!(
                view.review.notebook().unwrap().get(id).unwrap().comment,
                "Spell it out"
            );
            assert_eq!(view.review.note_count(), 1);
        })
    });
    // The note survives switching mode and a change to another checkout.
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.set_review_mode(Mode::Branch, cx);
            assert_eq!(view.review.note_count(), 1);
            view.set_review_mode(Mode::Uncommitted, cx);
            let other = crate::pull_request::Input {
                checkout: None,
                repo_key: "/elsewhere/.git".into(),
                branch: "main".into(),
            };
            view.review
                .track(Some(Source::Worktree(other)), false, Instant::now());
            assert_eq!(view.review.note_count(), 0);
            view.review.track(
                Some(Source::Worktree(repo.input.clone())),
                true,
                Instant::now(),
            );
            assert_eq!(view.review.note_count(), 1);
        })
    });
    wait_for(&view, cx, "the listing again", |review| {
        review.listing().is_some()
    });
    // The lines change under the note: it is stale, but kept.
    repo.write("tracked.txt", "one\nTWO\nthree\n");
    cx.update(|_, cx| view.update(cx, |view, _| view.review.refresh()));
    wait_for(&view, cx, "the changed listing", |review| {
        review
            .listing()
            .is_some_and(|listing| listing.entry("tracked.txt").is_some())
    });
    select(&view, cx, "tracked.txt");
    wait_for(&view, cx, "the new diff", |review| {
        review
            .loaded()
            .is_some_and(|loaded| loaded.diff.rows.iter().any(|row| row.text() == Some("TWO")))
    });
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            assert!(view.review.is_stale(id));
            assert_eq!(view.review.note_count(), 1);
            view.review.notes_expanded = true;
            cx.notify();
        })
    });
    draw(cx);
    assert!(
        cx.debug_bounds("review-note-1").is_none(),
        "a stale note leaves the diff"
    );
    assert!(cx.debug_bounds("review-note-stale").is_some());
    cx.update(|_, cx| view.update(cx, |view, cx| view.review_remove_note(id, cx)));
    view.read_with(cx, |view, _| assert_eq!(view.review.note_count(), 0));
}

#[gpui::test]
fn comments_are_sent_to_an_agent_or_copied_when_none_can_take_them(cx: &mut TestAppContext) {
    let repo = Repo::new();
    repo.write("tracked.txt", "one\n2\nthree\n");
    let (view, cx) = window(cx);
    open_on(&repo, &view, cx);
    select(&view, cx, "tracked.txt");
    // The fixture's agents have no panes: nothing can take the review, so it
    // is copied and the comments are handed off.
    add_note(&view, cx, (2, 3), "Rename this");
    cx.update(|_, cx| view.update(cx, |view, cx| view.send_review(None, cx)));
    let text = clipboard(cx).unwrap();
    assert!(
        text.contains("`tracked.txt`:L2\n```diff\n-two\n+2\n```\nComment: Rename this\n"),
        "{text}"
    );
    assert!(text.contains("(branch `feature`)"), "{text}");
    view.read_with(cx, |view, _| {
        assert_eq!(view.review.note_count(), 0);
        assert_eq!(view.deliveries.len(), 0);
        assert!(view.flash.as_ref().unwrap().0.text.contains("copied"));
    });
    // A shell in the pane is never typed into.
    with_pane(&view, cx, None);
    add_note(&view, cx, (2, 2), "Shell bound");
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            assert!(view.review_candidates().is_empty());
            view.send_review(Some("w0:p1".into()), cx);
            assert_eq!(view.deliveries.len(), 0);
        })
    });
    assert!(clipboard(cx).unwrap().contains("Shell bound"));
    // A working agent's pane takes it once the agent is idle; without a
    // connection here the paste fails and the clipboard gets it instead.
    with_pane(&view, cx, Some("working"));
    add_note(&view, cx, (3, 3), "Agent bound");
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            let candidates = view.review_candidates();
            assert_eq!(candidates.len(), 1);
            assert!(candidates[0].local);
            assert_eq!(view.review_target().unwrap().pane_id, "w0:p1");
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()));
            view.send_review(None, cx);
            assert_eq!(view.deliveries.len(), 1);
            assert_eq!(view.review.note_count(), 0, "handed off at once");
            assert!(
                view.flash
                    .as_ref()
                    .unwrap()
                    .0
                    .text
                    .contains("once it is idle")
            );
            view.poll_deliveries(cx);
            assert_eq!(view.deliveries.len(), 1, "held while the agent works");
        })
    });
    assert_eq!(clipboard(cx).as_deref(), Some("untouched"));
    with_pane(&view, cx, Some("idle"));
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.poll_deliveries(cx);
            assert_eq!(view.deliveries.len(), 0);
        })
    });
    assert!(clipboard(cx).unwrap().contains("Comment: Agent bound"));
    // The picker lists the agent and sends to the one picked.
    add_note(&view, cx, (1, 1), "Picked");
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.review.picker_open = true;
            cx.notify();
        })
    });
    draw(cx);
    assert!(cx.debug_bounds("review-agent-picker").is_some());
    let row = cx.debug_bounds("review-agent-0").unwrap();
    cx.simulate_click(row.center(), Default::default());
    view.read_with(cx, |view, _| {
        assert!(!view.review.picker_open);
        assert_eq!(view.review.chosen_pane.as_deref(), Some("w0:p1"));
        assert_eq!(view.deliveries.len(), 1);
    });
}

#[gpui::test]
fn copy_keeps_the_comments_and_clear_drops_them(cx: &mut TestAppContext) {
    let repo = Repo::new();
    repo.write("tracked.txt", "one\n2\nthree\n");
    let (view, cx) = window(cx);
    open_on(&repo, &view, cx);
    select(&view, cx, "tracked.txt");
    add_note(&view, cx, (3, 3), "Copy me");
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.copy_review(cx);
            assert_eq!(view.review.note_count(), 1, "copying keeps the comments");
        })
    });
    assert!(clipboard(cx).unwrap().contains("Comment: Copy me"));
    cx.update(|_, cx| view.update(cx, |view, cx| view.clear_review(cx)));
    view.read_with(cx, |view, _| assert_eq!(view.review.note_count(), 0));
    cx.update(|_, cx| {
        view.update(cx, |view, cx| {
            view.copy_review(cx);
            assert!(
                view.flash
                    .as_ref()
                    .unwrap()
                    .0
                    .text
                    .contains("No review comments")
            );
        })
    });
}
