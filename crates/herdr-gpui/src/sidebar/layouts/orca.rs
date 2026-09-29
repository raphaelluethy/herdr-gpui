//! Orca's worktree cards: a status column beside the name, a quieter meta line
//! with the host, branch, and pull request, and compact single-line agents.
//! Cards are inset and rounded; the focused one is washed and outlined.

use super::{
    super::{
        agents::status_style,
        cell::{AgentRow, RowContext, RowLayout, RowState, WorkspaceRow},
        line_height,
        row::RowTree,
    },
    parts::{self, Line, glyph_at, wash},
};
use crate::config::Theme;
use gpui::{prelude::*, *};

pub(in super::super) struct Orca;

/// Space between a card and the sidebar's edges.
const MARGIN: f32 = 4.;
const BORDER: f32 = 1.;
/// The status column, wider than its dot so the dot has air on both sides.
const STATUS_COLUMN: f32 = 12.;
const GAP: f32 = 6.;

/// The card or agent strip: inset, rounded, and marked by state. The border
/// is always present, transparent unless selected, so selecting a row never
/// shifts its content, and it keeps neighbouring cards apart without vertical
/// margins, which a dragged row's shifts could not see.
fn card(key: &str, state: RowState, indent: f32, radius: f32, theme: &Theme) -> Div {
    let card = div()
        .debug_selector(|| format!("row-{key}"))
        .relative()
        .flex_none()
        .ml(px(MARGIN + indent))
        .mr(px(MARGIN))
        .rounded(px(radius))
        .border_1()
        .border_color(rgba(0))
        .when(state.selected, |card| {
            card.border_color(wash(theme.foreground, 0x2e))
        })
        .flex()
        .gap(px(GAP))
        .px(px(GAP))
        .cursor_pointer();
    parts::mark(
        card,
        state,
        (wash(theme.foreground, 0x1a), wash(theme.foreground, 0x0a)),
        theme,
    )
}

/// Width inside a card's margins, border, and padding, less its status
/// column and the gap after it.
fn inner(cx: &RowContext<'_>, indent: f32) -> f32 {
    cx.width - 1. - 2. * MARGIN - indent - 2. * BORDER - 2. * GAP - STATUS_COLUMN - GAP
}

fn status_column(child: impl IntoElement) -> Div {
    div()
        .w(px(STATUS_COLUMN))
        .flex_none()
        .flex()
        .justify_center()
        .child(child)
}

impl RowLayout for Orca {
    fn workspace(&self, row: WorkspaceRow<'_>, state: RowState, cx: &RowContext<'_>) -> Div {
        let (branch, status) = (row.branch(), row.status());
        let WorkspaceRow {
            label,
            tree,
            fold,
            grouped,
            badge,
            removing,
            ..
        } = row;
        let (theme, font) = (cx.theme, cx.font);
        let line = line_height(font);
        let small = (font.size * 0.85).round();
        let glyph = glyph_at(font, small);
        let indent = if tree == RowTree::None {
            0.
        } else {
            cx.look.density.padding()
        };
        let width = inner(cx, indent);
        // Children are named by their branch already; repeating it is noise.
        let branch = branch.filter(|branch| *branch != label);
        let pr = badge.as_ref().and_then(|badge| badge.pr.as_ref());
        let dirty = badge.as_ref().is_some_and(|badge| badge.dirty);
        let dirty_size = (line * 0.7).round().min(14.);
        let meta = branch.is_some() || cx.host.is_some() || badge.is_some();
        // The repository's own checkout is the group's primary card.
        let primary = (grouped && tree == RowTree::None).then(|| {
            div()
                .flex()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(wash(theme.foreground, 0x33))
                .bg(wash(theme.foreground, 0x0f))
                .text_size(px(small))
                .text_color(rgb(theme.subtext()))
                .child(super::super::label_text("primary"))
        });
        let title = Line::new(width, GAP)
            .fill(
                div()
                    .debug_selector(|| format!("name-{label}"))
                    .h(px(line))
                    .text_color(rgb(theme.foreground))
                    .when(state.selected, |title| {
                        title.font_weight(FontWeight::SEMIBOLD)
                    }),
                label,
            )
            .when_some(primary, |line, primary| {
                let width = (7. * glyph).ceil() + 10.;
                line.fixed(width, primary.w(px(width)))
            })
            .when_some(fold, |line, fold| {
                line.fixed(14., fold.element(theme).w(px(14.)).text_size(px(14.)))
            });
        let details = Line::new(width, GAP)
            .when_some(cx.host, |line, host| {
                let chip = div().rounded(px(4.)).bg(rgb(theme.active));
                line.padded(chip, host, glyph, 1. / 3., 5.)
            })
            .map(|line| match branch {
                Some(branch) => {
                    line.fill(div().debug_selector(|| format!("detail-{label}")), branch)
                }
                None => line.spacer(),
            })
            .when(dirty, |line| {
                line.fixed(dirty_size, parts::dirty(label, dirty_size, theme))
            })
            .when_some(pr, |line, pr| {
                let (width, number) = parts::pr_number(label, pr, small, glyph);
                line.shrink(width, number)
            });
        card(label, state, indent, 8., theme)
            .items_start()
            .py(px(if meta { 5. } else { 7. }))
            .when(removing, |card| card.opacity(0.5))
            .child(
                status_column(parts::status(status, removing, theme, font))
                    .h(px(line))
                    .items_center(),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(title.into_div().h(px(line)))
                    .when(meta, |column| {
                        column.child(
                            details
                                .into_div()
                                .h(px(line))
                                .text_size(px(small))
                                .text_color(rgb(theme.muted)),
                        )
                    }),
            )
    }

    fn agent(&self, agent: AgentRow<'_>, state: RowState, cx: &RowContext<'_>) -> Div {
        let (theme, font) = (cx.theme, cx.font);
        let key = agent.key.as_str();
        let line = line_height(font);
        let icon = (line * 0.85).round();
        let color = if state.selected {
            theme.foreground
        } else {
            theme.subtext()
        };
        // The agent leads, then where it runs, all on one line.
        let text = Line::new(inner(cx, 0.) - icon - GAP, GAP)
            .fill(
                div()
                    .debug_selector(|| format!("name-{key}"))
                    .text_color(rgb(color)),
                agent.name,
            )
            .when_some(parts::agent_place(agent.place, cx.host), |line, place| {
                line.label(
                    div().text_color(rgb(theme.muted)),
                    place,
                    glyph_at(font, font.size),
                    0.5,
                )
            })
            .when_some(agent.status_text, |line, text| {
                line.label(
                    div()
                        .debug_selector(|| format!("status-{key}"))
                        .text_color(rgb(status_style(agent.status).2)),
                    text,
                    glyph_at(font, font.size),
                    0.5,
                )
            });
        card(key, state, 0., 4., theme)
            .h(px(line + 8.))
            .items_center()
            .child(status_column(parts::status(
                agent.status,
                false,
                theme,
                font,
            )))
            .child(parts::icon(agent.icon.path(), icon, color))
            .child(text.into_div())
    }
}
