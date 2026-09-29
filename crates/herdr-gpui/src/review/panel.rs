//! Painting the review panel from prepared state: the header, the file list,
//! the diff of the selected file with its comments, and the footer that sends
//! them. Both lists are virtualized, and nothing here asks Git for anything.
use super::{
    MIN_CONTENT_WIDTH, MIN_WIDTH, Mode, Review,
    diff::{FileDiff, Row},
    notes::Note,
    status::{Entry, Kind, Section, Staged},
    view::open_target,
    worker::Listing,
};
use crate::{HerdrWindow, config::Theme, fonts::StyledFont};
use gpui::{prelude::*, *};
use std::{ops::RangeInclusive, sync::Arc};

/// Alpha of the added and removed line washes, as Zed draws them.
const LINE_WASH: u32 = 0x29;
/// Rows the file list shows before it scrolls.
const FILE_ROWS_SHOWN: usize = 8;

/// One row of the file list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FileRow {
    Header(Section),
    Entry(usize),
}

/// One row of the diff list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Line {
    Row(usize),
    Note(u64),
    Draft,
}

fn wash(color: u32, alpha: u32) -> Rgba {
    rgba((color << 8) | alpha)
}

fn kind_color(kind: Kind, theme: &Theme) -> u32 {
    match kind {
        Kind::Deleted => theme.palette[1],
        Kind::Added | Kind::Untracked => theme.palette[2],
        Kind::Conflict | Kind::Modified | Kind::Renamed | Kind::TypeChanged => theme.palette[3],
    }
}

/// The rows of the file list: each section that has files, then its files.
fn file_rows(listing: &Listing) -> Vec<FileRow> {
    let mut rows = Vec::with_capacity(listing.entries.len() + 3);
    let mut current = None;
    for (index, entry) in listing.entries.iter().enumerate() {
        let section = entry.section();
        if current != Some(section) {
            rows.push(FileRow::Header(section));
            current = Some(section);
        }
        rows.push(FileRow::Entry(index));
    }
    rows
}

/// What a section's checkbox shows: staged when every file is, unstaged when
/// none is, partial otherwise.
fn section_staged(listing: &Listing, section: Section) -> Staged {
    let mut any = false;
    let mut all = true;
    for entry in listing
        .entries
        .iter()
        .filter(|entry| entry.section() == section)
    {
        match entry.staged() {
            Staged::All => any = true,
            Staged::None => all = false,
            Staged::Partial => {
                any = true;
                all = false;
            }
        }
    }
    match (any, all) {
        (_, true) => Staged::All,
        (false, _) => Staged::None,
        (true, false) => Staged::Partial,
    }
}

/// The diff's rows with each note under the last row it covers and the draft
/// under its own.
fn diff_lines(
    diff: &FileDiff,
    anchors: &[(u64, Option<RangeInclusive<usize>>)],
    draft: Option<&RangeInclusive<usize>>,
) -> Vec<Line> {
    let mut lines = Vec::with_capacity(diff.rows.len() + anchors.len() + 1);
    for index in 0..diff.rows.len() {
        lines.push(Line::Row(index));
        for (id, rows) in anchors {
            if rows.as_ref().is_some_and(|rows| *rows.end() == index) {
                lines.push(Line::Note(*id));
            }
        }
        if draft.is_some_and(|rows| *rows.end() == index) {
            lines.push(Line::Draft);
        }
    }
    lines
}

fn checkbox(
    id: impl Into<ElementId>,
    staged: Staged,
    enabled: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let fill = matches!(staged, Staged::All | Staged::Partial);
    div()
        .id(id)
        .flex_none()
        .size(px(14.))
        .rounded(px(3.))
        .border_1()
        .border_color(rgb(if fill { theme.primary() } else { theme.muted }))
        .when(fill, |box_| box_.bg(rgb(theme.primary())))
        .when(!enabled, |box_| box_.opacity(0.5))
        .when(enabled, |box_| box_.cursor_pointer())
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(10.))
        .text_color(rgb(theme.text_on(theme.primary())))
        .when(staged == Staged::All, |box_| box_.child("\u{2713}"))
        .when(staged == Staged::Partial, |box_| box_.child("\u{2013}"))
}

fn icon_button(
    id: &'static str,
    path: &'static str,
    theme: &Theme,
    enabled: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex_none()
        .size(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(crate::config::corners::CONTROL))
        .when(enabled, |button| {
            button.cursor_pointer().hover(|s| s.bg(rgb(theme.active)))
        })
        .child(svg().path(path).size(px(13.)).text_color(rgb(if enabled {
            theme.foreground
        } else {
            theme.muted
        })))
}

fn text_button(
    id: &'static str,
    label: impl Into<SharedString>,
    primary: bool,
    theme: &Theme,
) -> Stateful<Div> {
    let background = if primary {
        theme.primary()
    } else {
        theme.active
    };
    div()
        .id(id)
        .debug_selector(move || id.into())
        .px_2()
        .py_1()
        .rounded(px(crate::config::corners::CONTROL))
        .cursor_pointer()
        .bg(rgb(background))
        .text_color(rgb(theme.text_on(background)))
        .whitespace_nowrap()
        .child(label.into())
}

impl HerdrWindow {
    /// The panel's width on this viewport: what the user set, kept from
    /// squeezing the terminal out.
    pub(crate) fn review_width(&self, viewport: f32) -> f32 {
        let sidebar = if self.sidebar_visible {
            crate::sidebar::sidebar_width(self.sidebar_width, viewport)
        } else {
            0.
        };
        let most = (viewport - sidebar - MIN_CONTENT_WIDTH).max(MIN_WIDTH);
        self.review.width.clamp(MIN_WIDTH, most)
    }

    pub(crate) fn render_review_panel(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.review.open {
            return None;
        }
        let theme = self.theme.clone();
        let width = self.review_width(f32::from(window.viewport_size().width));
        let view = cx.entity().downgrade();
        let panel = div()
            .id("review-panel")
            .debug_selector(|| "review-panel".into())
            .relative()
            .flex_none()
            .w(px(width))
            .h_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(theme.surface))
            .border_l_1()
            .border_color(rgb(theme.active))
            .track_focus(&self.review.focus)
            .on_key_down(cx.listener(Self::review_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // A field of the panel takes the click first and stops it;
                    // anywhere else the panel itself takes the keyboard.
                    window.focus(&this.review.focus, cx);
                    cx.notify();
                }),
            )
            .child(self.render_review_header(&theme, cx));
        let panel = match self.review.checkout().cloned() {
            None if self.review.finding() => panel.child(
                div()
                    .debug_selector(|| "review-finding".into())
                    .p_3()
                    .text_color(rgb(theme.muted))
                    .child("Finding the checkout\u{2026}"),
            ),
            None => panel.child(
                div()
                    .debug_selector(|| "review-unavailable".into())
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_color(rgb(theme.muted))
                    .child(
                        "Review needs a local Git checkout. Focus a tab whose pane works in a Git repository on this machine; remote and SSH workspaces are not supported yet.",
                    )
                    .when_some(self.review.error(), |note, error| {
                        note.child(
                            div()
                                .debug_selector(|| "review-unavailable-reason".into())
                                .child(error.to_owned()),
                        )
                    }),
            ),
            Some(_) => panel
                .child(self.render_review_files(&theme, cx))
                .child(self.render_review_diff(&theme, cx))
                .child(self.render_review_footer(&theme, cx)),
        };
        let drag_view = view.clone();
        let panel = panel
            .child(
                div()
                    .id("review-resize")
                    .debug_selector(|| "review-resize".into())
                    .absolute()
                    .left_0()
                    .top_0()
                    .h_full()
                    .w(px(6.))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .hover(|s| s.bg(rgba(0x78a9ff44)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if event.click_count == 2 {
                                this.review.width = super::DEFAULT_WIDTH;
                                this.review.drag = None;
                            } else {
                                this.review.drag = Some((f32::from(event.position.x), width));
                            }
                            cx.notify();
                        }),
                    ),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, _| {
                        // Captured at the window so the edge drag and a comment
                        // drag both continue outside the rows they started on.
                        let moving = drag_view.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase != DispatchPhase::Capture {
                                return;
                            }
                            let _ = moving.update(cx, |this, cx| {
                                if let Some((start, width)) = this.review.drag {
                                    let viewport = f32::from(window.viewport_size().width);
                                    let wanted = width + start - f32::from(event.position.x);
                                    this.review.width = wanted.clamp(MIN_WIDTH, viewport);
                                    cx.stop_propagation();
                                    cx.notify();
                                } else if this.review.dragging
                                    && event.pressed_button != Some(MouseButton::Left)
                                {
                                    this.review.dragging = false;
                                }
                            });
                        });
                        let released = drag_view.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Capture && event.button == MouseButton::Left
                            {
                                let _ = released.update(cx, |this, cx| {
                                    if this.review.drag.take().is_some() {
                                        cx.stop_propagation();
                                        cx.notify();
                                    }
                                    this.review.dragging = false;
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            );
        Some(panel.into_any_element())
    }

    fn render_review_header(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let review = &self.review;
        let listing = review.listing();
        let mode_button = |mode: Mode, current: Mode| {
            let on = mode == current;
            div()
                .id(match mode {
                    Mode::Uncommitted => "review-mode-uncommitted",
                    Mode::Branch => "review-mode-branch",
                })
                .debug_selector(move || match mode {
                    Mode::Uncommitted => "review-mode-uncommitted".into(),
                    Mode::Branch => "review-mode-branch".into(),
                })
                .px_2()
                .py(px(2.))
                .rounded(px(crate::config::corners::CONTROL))
                .cursor_pointer()
                .when(on, |button| {
                    button
                        .bg(rgb(theme.primary_wash()))
                        .text_color(rgb(theme.text_on(theme.primary_wash())))
                })
                .when(!on, |button| {
                    button
                        .text_color(rgb(theme.muted))
                        .hover(|s| s.bg(rgb(theme.active)))
                })
                .child(mode.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.set_review_mode(mode, cx);
                }))
        };
        let refreshing = review.refreshing();
        let mut header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(rgb(theme.active))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Review"))
                    .when_some(review.checkout(), |row, checkout| {
                        row.child(
                            div()
                                .debug_selector(|| "review-branch".into())
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(theme.muted))
                                .child(checkout.branch.clone()),
                        )
                    })
                    .when(review.checkout().is_none(), |row| row.child(div().flex_1()))
                    .child(
                        icon_button("review-refresh", "icons/refresh.svg", theme, !refreshing)
                            .when(refreshing, |button| button.opacity(0.6))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.review.refresh();
                                cx.notify();
                            })),
                    )
                    .child(
                        icon_button("review-close", "icons/close.svg", theme, true).on_click(
                            cx.listener(|this, _, window, cx| {
                                cx.stop_propagation();
                                this.toggle_review(window, cx);
                            }),
                        ),
                    ),
            );
        if review.checkout().is_some() {
            let mode = review.mode();
            let mut controls = div()
                .flex()
                .items_center()
                .gap_1()
                .child(mode_button(Mode::Uncommitted, mode))
                .child(mode_button(Mode::Branch, mode))
                .child(div().flex_1());
            if let Some(listing) = listing {
                if listing.mode == Mode::Uncommitted && !listing.entries.is_empty() {
                    let any_unstaged = listing
                        .entries
                        .iter()
                        .any(|entry| entry.staged() != Staged::All);
                    let writing = review.writing();
                    let (label, stage) = if any_unstaged {
                        ("Stage all", true)
                    } else {
                        ("Unstage all", false)
                    };
                    controls = controls.child(
                        text_button("review-stage-all", label, false, theme)
                            .when(writing, |button| button.opacity(0.5))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.review_stage_all(stage, cx);
                            })),
                    );
                }
                controls =
                    controls
                        .child(
                            div()
                                .debug_selector(|| "review-stat".into())
                                .flex()
                                .gap_1()
                                .child(div().text_color(rgb(theme.palette[2])).child(format!(
                                    "+{}",
                                    crate::sidebar::compact(listing.additions)
                                )))
                                .child(div().text_color(rgb(theme.palette[1])).child(format!(
                                    "-{}",
                                    crate::sidebar::compact(listing.deletions)
                                ))),
                        )
                        .child(div().text_color(rgb(theme.muted)).child(format!(
                            "{} {}",
                            listing.entries.len(),
                            if listing.entries.len() == 1 {
                                "file"
                            } else {
                                "files"
                            }
                        )));
            }
            header = header.child(controls);
            if let Some(base) = listing.and_then(|listing| listing.base.as_ref()) {
                header = header.child(
                    div()
                        .debug_selector(|| "review-base".into())
                        .text_color(rgb(theme.muted))
                        .truncate()
                        .child(format!("Changes since {}", base.name)),
                );
            }
            if listing.is_some_and(|listing| listing.truncated) {
                header = header.child(
                    div()
                        .text_color(rgb(theme.palette[3]))
                        .child("Only the first 2000 files are listed"),
                );
            }
        }
        if let Some(error) = review.error() {
            header = header.child(
                div()
                    .debug_selector(|| "review-error".into())
                    .text_color(rgb(theme.palette[1]))
                    .child(error.to_owned()),
            );
        }
        header
    }

    fn render_review_files(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let review = &self.review;
        let row_height = self.config.ui.line_height() + 8.;
        let Some(listing) = review.listing().cloned() else {
            return div().flex_none().p_3().text_color(rgb(theme.muted)).child(
                if review.error().is_some() {
                    "Could not read the checkout"
                } else {
                    "Reading the working tree\u{2026}"
                },
            );
        };
        if listing.entries.is_empty() {
            return div()
                .debug_selector(|| "review-empty".into())
                .flex_none()
                .p_3()
                .border_b_1()
                .border_color(rgb(theme.active))
                .text_color(rgb(theme.muted))
                .child(match listing.mode {
                    Mode::Uncommitted => "No uncommitted changes",
                    Mode::Branch => "No changes since the base branch",
                });
        }
        let rows = Arc::new(file_rows(&listing));
        let count = rows.len();
        let selected = review.selected().map(str::to_owned);
        let stageable = listing.mode == Mode::Uncommitted && !review.writing();
        let view = cx.entity().downgrade();
        let border = theme.active;
        let theme = theme.clone();
        let font = self.config.ui.clone();
        let list = uniform_list("review-files", count, move |range, _, _| {
            range
                .filter_map(|index| {
                    let row = rows.get(index)?.clone();
                    let view = view.clone();
                    let element =
                        match row {
                            FileRow::Header(section) => {
                                let staged = section_staged(&listing, section);
                                let files = listing
                                    .entries
                                    .iter()
                                    .filter(|entry| entry.section() == section)
                                    .count();
                                let stage = staged != Staged::All;
                                div()
                                    .id(("review-section", index))
                                    .w_full()
                                    .h(px(row_height))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .bg(rgb(theme.background))
                                    .text_color(rgb(theme.muted))
                                    .child(
                                        div()
                                            .debug_selector(move || {
                                                format!("review-section-{}", section.label())
                                            })
                                            .flex_1()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(format!("{} ({files})", section.label())),
                                    )
                                    .when(
                                        listing.mode == Mode::Uncommitted
                                            && section != Section::Conflicts,
                                        |header| {
                                            header.child(
                                                checkbox(
                                                    ("review-section-stage", index),
                                                    staged,
                                                    stageable,
                                                    &theme,
                                                )
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    view.update(cx, |this, cx| {
                                                        this.review_stage_section(
                                                            section, stage, cx,
                                                        );
                                                    })
                                                    .ok();
                                                }),
                                            )
                                        },
                                    )
                                    .into_any_element()
                            }
                            FileRow::Entry(position) => {
                                let entry = listing.entries.get(position)?.clone();
                                let path = entry.path.clone();
                                let is_selected = selected.as_deref() == Some(path.as_str());
                                let kind = entry.kind();
                                let (name, dir) = entry.name_and_dir();
                                let name = crate::pull_request::clean(name);
                                let dir = crate::pull_request::clean(dir);
                                let openable = open_target(&listing, &path).is_some();
                                let staged = entry.staged();
                                let select_path = path.clone();
                                let stage_path = path.clone();
                                let open_path = path.clone();
                                let stage_view = view.clone();
                                let open_view = view.clone();
                                div()
                                    .id(("review-file", index))
                                    .w_full()
                                    .debug_selector(move || format!("review-file-{position}"))
                                    .group("review-file")
                                    .h(px(row_height))
                                    .px_2()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .cursor_pointer()
                                    .when(is_selected, |row| row.bg(rgb(theme.primary_wash())))
                                    .when(!is_selected, |row| {
                                        row.hover(|s| s.bg(rgb(theme.active)))
                                    })
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        view.update(cx, |this, cx| {
                                            this.review_select(&select_path, window, cx);
                                        })
                                        .ok();
                                    })
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(12.))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(rgb(kind_color(kind, &theme)))
                                            .child(kind.letter()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_baseline()
                                            .gap_1()
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .max_w_full()
                                                    .truncate()
                                                    .when(kind == Kind::Deleted, |name| {
                                                        name.line_through()
                                                    })
                                                    .child(name),
                                            )
                                            .when(!dir.is_empty(), |row| {
                                                row.child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_color(rgb(theme.muted))
                                                        .child(dir),
                                                )
                                            }),
                                    )
                                    .when(openable, |row| {
                                        row.child(
                                            div()
                                                .id(("review-file-open", index))
                                                .flex_none()
                                                .size(px(18.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(crate::config::corners::CONTROL))
                                                .opacity(0.)
                                                .group_hover("review-file", |s| s.opacity(1.))
                                                .hover(|s| s.bg(rgb(theme.active)))
                                                .child(
                                                    svg()
                                                        .path("icons/external.svg")
                                                        .size(px(12.))
                                                        .text_color(rgb(theme.muted)),
                                                )
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    open_view
                                                        .update(cx, |this, cx| {
                                                            this.review_open_file(&open_path, cx)
                                                        })
                                                        .ok();
                                                }),
                                        )
                                    })
                                    .when(
                                        listing.mode == Mode::Uncommitted && kind != Kind::Conflict,
                                        |row| {
                                            row.child(
                                                checkbox(
                                                    ("review-file-stage", index),
                                                    staged,
                                                    stageable,
                                                    &theme,
                                                )
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    stage_view
                                                        .update(cx, |this, cx| {
                                                            this.review_toggle_staged(
                                                                &stage_path,
                                                                cx,
                                                            )
                                                        })
                                                        .ok();
                                                }),
                                            )
                                        },
                                    )
                                    .into_any_element()
                            }
                        };
                    Some(element)
                })
                .collect()
        })
        .track_scroll(&review.file_scroll)
        .h(px(row_height * count.min(FILE_ROWS_SHOWN) as f32))
        .w_full()
        .text_font(&font);
        div()
            .debug_selector(|| "review-files".into())
            .flex_none()
            .border_b_1()
            .border_color(rgb(border))
            .child(list)
    }

    fn render_review_diff(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let review = &self.review;
        let body = div()
            .debug_selector(|| "review-diff".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col();
        let Some((listing, path)) = review
            .listing()
            .cloned()
            .zip(review.selected().map(str::to_owned))
        else {
            return body.child(
                div()
                    .p_3()
                    .text_color(rgb(theme.muted))
                    .child("Select a file to see its changes"),
            );
        };
        let Some(entry) = listing.entry(&path).cloned() else {
            return body;
        };
        let body = body.child(self.render_review_diff_header(&listing, &entry, theme, cx));
        let Some(loaded) = review.loaded().filter(|loaded| loaded.path == path) else {
            return body.child(div().p_3().text_color(rgb(theme.muted)).child(
                if review.loading() == Some(path.as_str()) {
                    "Loading the diff\u{2026}"
                } else {
                    "No diff loaded"
                },
            ));
        };
        let diff = loaded.diff.clone();
        let mut body = body;
        let notice = |text: &'static str| {
            div()
                .debug_selector(move || format!("review-notice-{text}"))
                .px_3()
                .py_1()
                .text_color(rgb(theme.palette[3]))
                .child(text)
        };
        if diff.binary {
            body = body.child(notice("Binary file"));
        }
        if diff.metadata_only {
            body = body.child(notice("Only the file's mode or name changed"));
        }
        if diff.is_empty() {
            body = body.child(notice("No changes to show"));
        }
        if diff.truncated {
            body = body.child(notice("Diff truncated"));
        }
        if diff.rows.is_empty() {
            return body;
        }
        let draft = review
            .draft()
            .filter(|draft| draft.path == path)
            .map(|draft| draft.rows.clone());
        let lines = Arc::new(diff_lines(&diff, &loaded.anchors, draft.as_ref()));
        let notes: Arc<Vec<Note>> = Arc::new(
            review
                .notebook()
                .map(|book| book.on_path(&path).cloned().collect())
                .unwrap_or_default(),
        );
        let count = lines.len();
        let row_height = self.config.ui.line_height() + 10.;
        let theme = theme.clone();
        let ui = self.config.ui.clone();
        let mono = self.config.terminal.clone();
        let input = review.input.clone();
        let editing = review.draft().and_then(|draft| draft.editing);
        let view = cx.entity().downgrade();
        let list = uniform_list("review-diff-lines", count, move |range, _, _| {
            range
                .filter_map(|index| {
                    let line = lines.get(index)?.clone();
                    let view = view.clone();
                    let element = match line {
                        Line::Row(row_index) => {
                            let row = diff.rows.get(row_index)?;
                            let in_draft =
                                draft.as_ref().is_some_and(|rows| rows.contains(&row_index));
                            let (sign, background, sign_color) = match row {
                                Row::Added { .. } => (
                                    "+",
                                    Some(wash(theme.palette[2], LINE_WASH)),
                                    theme.palette[2],
                                ),
                                Row::Removed { .. } => (
                                    "-",
                                    Some(wash(theme.palette[1], LINE_WASH)),
                                    theme.palette[1],
                                ),
                                Row::Hunk(_) => {
                                    ("", Some(rgba((theme.active << 8) | 0xff)), theme.muted)
                                }
                                Row::Context { .. } | Row::NoNewline => (" ", None, theme.muted),
                            };
                            let number = |value: Option<u32>| {
                                div()
                                    .flex_none()
                                    .w(px(34.))
                                    .text_color(rgb(theme.muted))
                                    .text_size(px(ui.size * 0.85))
                                    .child(value.map(|value| value.to_string()).unwrap_or_default())
                            };
                            let text: SharedString = match row {
                                Row::Hunk(header) => header.clone().into(),
                                Row::NoNewline => "\\ No newline at end of file".into(),
                                _ => row.text().unwrap_or_default().to_owned().into(),
                            };
                            let is_line = row.is_line();
                            let plus_view = view.clone();
                            div()
                                .id(("review-line", index))
                                .w_full()
                                .debug_selector(move || format!("review-line-{row_index}"))
                                .group("review-line")
                                .h(px(row_height))
                                .flex()
                                .items_center()
                                .when_some(background, |row, background| row.bg(background))
                                .when(in_draft, |row| row.bg(wash(theme.primary(), 0x50)))
                                .on_mouse_move(move |event: &MouseMoveEvent, window, cx| {
                                    if event.pressed_button != Some(MouseButton::Left) {
                                        return;
                                    }
                                    view.update(cx, |this, cx| {
                                        if this.review.dragging && is_line {
                                            this.review_begin_draft(row_index, true, window, cx);
                                        }
                                    })
                                    .ok();
                                })
                                .child(
                                    div()
                                        .id(("review-line-add", index))
                                        .debug_selector(move || {
                                            format!("review-line-add-{row_index}")
                                        })
                                        .flex_none()
                                        .w(px(18.))
                                        .h_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .when(is_line, |gutter| {
                                            gutter
                                                .cursor_pointer()
                                                .opacity(if in_draft { 1. } else { 0. })
                                                .group_hover("review-line", |s| s.opacity(1.))
                                                .child(
                                                    svg()
                                                        .path("icons/plus.svg")
                                                        .size(px(11.))
                                                        .text_color(rgb(theme.primary())),
                                                )
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    move |event: &MouseDownEvent, window, cx| {
                                                        cx.stop_propagation();
                                                        let extend = event.modifiers.shift;
                                                        plus_view
                                                            .update(cx, |this, cx| {
                                                                this.review_begin_draft(
                                                                    row_index, extend, window, cx,
                                                                );
                                                                this.review.dragging = true;
                                                            })
                                                            .ok();
                                                    },
                                                )
                                        }),
                                )
                                .child(number(row.old_line()))
                                .child(number(row.new_line()))
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(12.))
                                        .text_color(rgb(sign_color))
                                        .child(sign),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_font(&mono)
                                        .text_size(px(ui.size))
                                        .when(
                                            matches!(row, Row::Hunk(_) | Row::NoNewline),
                                            |text| text.text_color(rgb(theme.muted)),
                                        )
                                        .child(text),
                                )
                                .into_any_element()
                        }
                        Line::Note(id) => {
                            let note = notes.iter().find(|note| note.id == id)?;
                            let label = note.anchor.lines_label();
                            let edit_view = view.clone();
                            let is_editing = editing == Some(id);
                            div()
                                .id(("review-note", index))
                                .w_full()
                                .debug_selector(move || format!("review-note-{id}"))
                                .h(px(row_height))
                                .px_2()
                                .flex()
                                .items_center()
                                .gap_2()
                                .bg(wash(theme.primary(), 0x30))
                                .border_l_2()
                                .border_color(rgb(theme.primary()))
                                .when(is_editing, |row| row.opacity(0.6))
                                .child(
                                    svg()
                                        .path("icons/pencil.svg")
                                        .size(px(12.))
                                        .flex_none()
                                        .text_color(rgb(theme.primary())),
                                )
                                .child(div().flex_none().text_color(rgb(theme.muted)).child(label))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(note.comment.clone()),
                                )
                                .child(
                                    icon_button(
                                        "review-note-edit",
                                        "icons/pencil.svg",
                                        &theme,
                                        true,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            edit_view
                                                .update(cx, |this, cx| {
                                                    this.review_edit_note(id, window, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                                .child(
                                    icon_button(
                                        "review-note-remove",
                                        "icons/trash.svg",
                                        &theme,
                                        true,
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.stop_propagation();
                                            view.update(cx, |this, cx| {
                                                this.review_remove_note(id, cx)
                                            })
                                            .ok();
                                        },
                                    ),
                                )
                                .into_any_element()
                        }
                        Line::Draft => {
                            let rows = draft.clone()?;
                            let label = draft_label(&diff, &rows);
                            let add_view = view.clone();
                            let cancel_view = view.clone();
                            div()
                                .id("review-draft")
                                .w_full()
                                .debug_selector(|| "review-draft".into())
                                .h(px(row_height))
                                .px_2()
                                .flex()
                                .items_center()
                                .gap_2()
                                .bg(wash(theme.primary(), 0x30))
                                .border_l_2()
                                .border_color(rgb(theme.primary()))
                                .on_key_down(move |event: &KeyDownEvent, window, cx| {
                                    match event.keystroke.key.as_str() {
                                        "enter" => {
                                            view.update(cx, |this, cx| {
                                                this.review_add_note(window, cx)
                                            })
                                            .ok();
                                        }
                                        "escape" => {
                                            view.update(cx, |this, cx| {
                                                this.review_cancel_draft(window, cx)
                                            })
                                            .ok();
                                        }
                                        _ => return,
                                    }
                                    cx.stop_propagation();
                                })
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(theme.muted))
                                        .whitespace_nowrap()
                                        .child(label),
                                )
                                .child(div().flex_1().min_w_0().child(input.clone()))
                                .child(
                                    text_button(
                                        "review-draft-add",
                                        if editing.is_some() { "Save" } else { "Add" },
                                        true,
                                        &theme,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            add_view
                                                .update(cx, |this, cx| {
                                                    this.review_add_note(window, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                                .child(
                                    icon_button(
                                        "review-draft-cancel",
                                        "icons/close.svg",
                                        &theme,
                                        true,
                                    )
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            cancel_view
                                                .update(cx, |this, cx| {
                                                    this.review_cancel_draft(window, cx)
                                                })
                                                .ok();
                                        },
                                    ),
                                )
                                .into_any_element()
                        }
                    };
                    Some(element)
                })
                .collect()
        })
        .track_scroll(&review.diff_scroll)
        .flex_1()
        .min_h_0()
        .w_full()
        .text_font(&self.config.ui);
        body.child(list)
    }

    fn render_review_diff_header(
        &self,
        listing: &Listing,
        entry: &Entry,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Div {
        let kind = entry.kind();
        let (name, dir) = entry.name_and_dir();
        let name = crate::pull_request::clean(name);
        let dir = crate::pull_request::clean(dir);
        let path = entry.path.clone();
        let openable = open_target(listing, &path).is_some();
        let open_path = path.clone();
        let reveal_path = path.clone();
        let stage_path = path.clone();
        let staged = entry.staged();
        let stageable = listing.mode == Mode::Uncommitted && kind != Kind::Conflict;
        let stage_label = if staged == Staged::All {
            "Unstage"
        } else {
            "Stage"
        };
        div()
            .debug_selector(|| "review-diff-header".into())
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(rgb(theme.active))
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .rounded(px(3.))
                    .text_size(px(self.config.ui.size * 0.85))
                    .bg(wash(kind_color(kind, theme), 0x40))
                    .text_color(rgb(kind_color(kind, theme)))
                    .child(kind.label()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_1()
                    .child(
                        div()
                            .flex_none()
                            .max_w_full()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(kind == Kind::Deleted, |name| name.line_through())
                            .child(name),
                    )
                    .when(!dir.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(theme.muted))
                                .child(dir),
                        )
                    }),
            )
            .when(stageable, |header| {
                let writing = self.review.writing();
                header.child(
                    text_button("review-diff-stage", stage_label, false, theme)
                        .when(writing, |button| button.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.review_toggle_staged(&stage_path, cx);
                        })),
                )
            })
            .child(
                icon_button("review-open", "icons/external.svg", theme, openable).on_click(
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.review_open_file(&open_path, cx);
                    }),
                ),
            )
            .child(
                text_button("review-reveal", "Reveal", false, theme)
                    .when(!openable, |button| button.opacity(0.5))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.review_reveal_file(&reveal_path, cx);
                    })),
            )
    }

    fn render_review_footer(&self, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let review = &self.review;
        let count = review.note_count();
        let candidates = self.review_candidates();
        let target = self.review_target();
        let mut footer = div()
            .debug_selector(|| "review-footer".into())
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(rgb(theme.active));
        if review.picker_open {
            let mut picker = div()
                .id("review-agent-picker")
                .debug_selector(|| "review-agent-picker".into())
                .flex()
                .flex_col()
                .max_h(px(180.))
                .overflow_y_scroll()
                .border_b_1()
                .border_color(rgb(theme.active));
            for (index, candidate) in candidates.iter().enumerate() {
                let pane = candidate.pane_id.clone();
                picker = picker.child(
                    div()
                        .id(("review-agent", index))
                        .debug_selector(move || format!("review-agent-{index}"))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme.active)))
                        .child(crate::sidebar::status_dot(candidate.status))
                        .child(div().flex_none().truncate().child(candidate.label.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(theme.muted))
                                .child(candidate.detail.clone()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.review.chosen_pane = Some(pane.clone());
                            this.send_review(Some(pane.clone()), cx);
                        })),
                );
            }
            if candidates.is_empty() {
                picker = picker.child(
                    div()
                        .p_2()
                        .text_color(rgb(theme.muted))
                        .child("No agent is open in a pane"),
                );
            }
            footer = footer.child(picker);
        }
        if review.notes_expanded && count > 0 {
            let notes: Vec<Note> = review
                .notebook()
                .map(|book| book.notes().to_vec())
                .unwrap_or_default();
            let mut list = div()
                .id("review-notes")
                .debug_selector(|| "review-notes".into())
                .flex()
                .flex_col()
                .max_h(px(200.))
                .overflow_y_scroll()
                .border_b_1()
                .border_color(rgb(theme.active));
            for (index, note) in notes.iter().enumerate() {
                let id = note.id;
                let stale = review.is_stale(id);
                let path = crate::pull_request::clean(&note.anchor.path);
                list = list.child(
                    div()
                        .id(("review-notes-row", index))
                        .debug_selector(move || format!("review-notes-row-{index}"))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme.active)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.review_show_note(id, cx);
                        }))
                        .child(
                            div()
                                .flex_none()
                                .text_color(rgb(theme.muted))
                                .child(format!("{path}:{}", note.anchor.lines_label())),
                        )
                        .when(stale, |row| {
                            row.child(
                                div()
                                    .debug_selector(|| "review-note-stale".into())
                                    .flex_none()
                                    .px_1()
                                    .rounded(px(3.))
                                    .bg(wash(theme.palette[3], 0x40))
                                    .text_color(rgb(theme.palette[3]))
                                    .child("stale"),
                            )
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(note.comment.clone()),
                        )
                        .child(
                            icon_button("review-notes-remove", "icons/trash.svg", theme, true)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.review_remove_note(id, cx);
                                })),
                        ),
                );
            }
            footer = footer.child(list);
        }
        let send_label = match &target {
            Some(candidate) => format!("Send to {} ({count})", candidate.label),
            None => format!("Send to agent ({count})"),
        };
        let has_notes = count > 0;
        footer.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .p_2()
                .child(
                    div()
                        .id("review-notes-toggle")
                        .debug_selector(|| "review-notes-toggle".into())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .cursor_pointer()
                        .text_color(rgb(theme.muted))
                        .child(format!(
                            "{count} {}",
                            if count == 1 { "comment" } else { "comments" }
                        ))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.review.notes_expanded = !this.review.notes_expanded;
                            cx.notify();
                        })),
                )
                .when(has_notes, |row| {
                    row.child(
                        text_button("review-send", send_label, true, theme)
                            .max_w(px(220.))
                            .truncate()
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.send_review(None, cx);
                            })),
                    )
                    .child(
                        div()
                            .id("review-send-pick")
                            .debug_selector(|| "review-send-pick".into())
                            .flex_none()
                            .px_1()
                            .py_1()
                            .rounded(px(crate::config::corners::CONTROL))
                            .cursor_pointer()
                            .bg(rgb(theme.active))
                            .hover(|s| s.bg(rgb(theme.primary_wash())))
                            .child(
                                svg()
                                    .path("icons/chevron-down.svg")
                                    .size(px(12.))
                                    .text_color(rgb(theme.foreground)),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.review.picker_open = !this.review.picker_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        text_button("review-copy", "Copy", false, theme).on_click(cx.listener(
                            |this, _, _, cx| {
                                cx.stop_propagation();
                                this.copy_review(cx);
                            },
                        )),
                    )
                    .child(
                        text_button("review-clear", "Clear", false, theme).on_click(cx.listener(
                            |this, _, _, cx| {
                                cx.stop_propagation();
                                this.clear_review(cx);
                            },
                        )),
                    )
                }),
        )
    }
}

/// "Lines 12-18" or "Line 12" for the rows a draft covers, counted on the
/// side a note there would take.
fn draft_label(diff: &FileDiff, rows: &RangeInclusive<usize>) -> String {
    match super::notes::Anchor::capture("", diff, rows.clone()) {
        Some(anchor) if anchor.lines.start() == anchor.lines.end() => {
            format!("Line {}", anchor.lines.start())
        }
        Some(anchor) => format!("Lines {}-{}", anchor.lines.start(), anchor.lines.end()),
        None => "Lines".into(),
    }
}

impl Review {
    /// The panel's rows for tests: how many the file list draws.
    #[cfg(test)]
    pub(crate) fn file_row_count(&self) -> usize {
        self.listing().map_or(0, |listing| file_rows(listing).len())
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would bring `gpui::test` in over `#[test]`.
    use super::{
        FileRow, Line, Mode, Section, Staged, diff_lines, draft_label, file_rows, section_staged,
        wash,
    };
    use crate::review::{diff::parse_unified, status::parse_porcelain, worker::Listing};
    use gpui::rgba;

    fn listing(status: &str) -> Listing {
        let mut entries = parse_porcelain(status);
        crate::review::status::sort_entries(&mut entries);
        Listing {
            mode: Mode::Uncommitted,
            root: "/repo".into(),
            head: Some("abc".into()),
            base: None,
            entries,
            truncated: false,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn the_file_list_groups_by_section_with_a_tri_state_header() {
        let mixed = listing("?? new\0M  staged\0 M unstaged\0UU conflict\0");
        let rows = file_rows(&mixed);
        assert_eq!(
            rows,
            [
                FileRow::Header(Section::Conflicts),
                FileRow::Entry(0),
                FileRow::Header(Section::Tracked),
                FileRow::Entry(1),
                FileRow::Entry(2),
                FileRow::Header(Section::Untracked),
                FileRow::Entry(3),
            ]
        );
        assert_eq!(section_staged(&mixed, Section::Tracked), Staged::Partial);
        assert_eq!(section_staged(&mixed, Section::Untracked), Staged::None);
        assert_eq!(
            section_staged(&listing("M  a\0A  b\0"), Section::Tracked),
            Staged::All
        );
        assert_eq!(
            section_staged(&listing("MM a\0"), Section::Tracked),
            Staged::Partial
        );
        assert!(file_rows(&listing("")).is_empty());
    }

    #[test]
    fn notes_and_the_draft_sit_under_the_rows_they_cover() {
        let diff = parse_unified("@@ -1,2 +1,3 @@\n a\n+b\n c\n");
        let lines = diff_lines(
            &diff,
            &[(7, Some(1..=2)), (8, None), (9, Some(2..=2))],
            Some(&(3..=3)),
        );
        assert_eq!(
            lines,
            [
                Line::Row(0),
                Line::Row(1),
                Line::Row(2),
                Line::Note(7),
                Line::Note(9),
                Line::Row(3),
                Line::Draft,
            ]
        );
        assert_eq!(draft_label(&diff, &(1..=1)), "Line 1");
        assert_eq!(draft_label(&diff, &(1..=3)), "Lines 1-3");
        assert_eq!(draft_label(&diff, &(0..=0)), "Lines");
        assert_eq!(wash(0x112233, 0x44), rgba(0x11223344));
    }
}
