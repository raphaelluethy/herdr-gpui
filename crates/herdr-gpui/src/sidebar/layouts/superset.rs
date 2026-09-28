//! Superset's single-line rows: one icon slot that reports the row's state,
//! the name, and the pull request's change counts on the right. The focused
//! row is filled and marked with a stripe down its leading edge.

use super::{
    super::{
        agents::status_style,
        cell::{AgentRow, RowContext, RowLayout, RowState, WorkspaceRow},
        line_height,
        row::{RowIcon, RowLift, RowTree, removing_dot},
    },
    parts::{self, Line, glyph_at, wash},
};
use crate::config::Theme;
use gpui::{prelude::*, *};
use herdr_client::protocol::AgentStatus;

pub(in super::super) struct Superset;

/// Width of the leading edge stripe on the focused row.
const STRIPE: f32 = 2.;

/// Geometry shared by both kinds of row, scaled from the sidebar font so a
/// larger font grows the icon slot with the text.
struct Metrics {
    icon: f32,
    glyph: f32,
    gap: f32,
    small: f32,
}

impl Metrics {
    fn new(cx: &RowContext<'_>) -> Self {
        let small = (cx.font.size * 0.8).round();
        Self {
            icon: (cx.font.size * 1.5).round().max(line_height(cx.font)),
            glyph: glyph_at(cx.font, small),
            gap: cx.look.density.gap(),
            small,
        }
    }
}

/// The row: padding, the state fill, the stripe, and one measured line.
fn shell(key: &str, state: RowState, indent: f32, line: Line<'_>, cx: &RowContext<'_>) -> Div {
    let theme = cx.theme;
    let gap = cx.look.density.gap();
    let row = div()
        .debug_selector(|| format!("row-{key}"))
        .relative()
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .py(px(gap))
        .pl(px(cx.look.content_x() + indent))
        .cursor_pointer();
    parts::mark(
        row,
        state,
        (rgb(theme.active), wash(theme.foreground, 0x0d)),
        theme,
    )
    // Flat rows round off while carried, so the card reads as picked up.
    .when(state.lift == RowLift::Lifted, |row| row.rounded(px(4.)))
    .when(state.selected, |row| {
        row.child(
            div()
                .debug_selector(|| format!("highlight-{key}"))
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(STRIPE))
                .rounded_r(px(STRIPE))
                .bg(rgb(theme.foreground)),
        )
    })
    .child(line.into_div())
}

/// The icon slot, with the status as a dot pinned to its top right corner.
/// Unknown draws no dot: there is nothing to report.
fn slot(
    key: &str,
    glyph: impl IntoElement,
    status: AgentStatus,
    m: &Metrics,
    theme: &Theme,
) -> Div {
    let dot = (status != AgentStatus::Unknown).then(|| {
        let (diameter, filled, color) = status_style(status);
        div()
            .absolute()
            .top(px(-2.))
            .right(px(-2.))
            .size(px(diameter))
            .rounded_full()
            .border_1()
            .border_color(rgb(color))
            .bg(rgb(if filled { color } else { theme.surface }))
    });
    div()
        .debug_selector(|| format!("icon-{key}"))
        .relative()
        .size(px(m.icon))
        .flex()
        .items_center()
        .justify_center()
        .child(glyph)
        .children(dot)
}

fn text_color(state: RowState, theme: &Theme) -> u32 {
    if state.selected {
        theme.foreground
    } else {
        theme.subtext()
    }
}

impl RowLayout for Superset {
    fn workspace(&self, row: WorkspaceRow<'_>, state: RowState, cx: &RowContext<'_>) -> Div {
        let status = row.status();
        let WorkspaceRow {
            label,
            tree,
            icon,
            fold,
            badge,
            removing,
            ..
        } = row;
        let theme = cx.theme;
        let m = Metrics::new(cx);
        let indent = if tree == RowTree::None {
            0.
        } else {
            cx.look.density.padding()
        };
        let pr = badge.as_ref().and_then(|badge| badge.pr.as_ref());
        let dirty = badge.as_ref().is_some_and(|badge| badge.dirty);
        let dirty_size = (line_height(cx.font) * 0.75).round().min(15.);
        let size = m.icon * 0.7;
        let icon_color = if state.selected {
            theme.foreground
        } else {
            theme.muted
        };
        // A pull request outranks the owner: its color is its state.
        let glyph = match (pr, icon) {
            (Some(pr), _) => parts::icon("icons/git-branch.svg", size, pr.color).into_any_element(),
            (None, RowIcon::None) => {
                parts::icon("icons/git-branch.svg", size, icon_color).into_any_element()
            }
            (None, icon) => div()
                .size(px(size))
                .child(icon.element(icon_color))
                .into_any_element(),
        };
        let slot = if removing {
            slot(
                label,
                removing_dot("worktree-removing", theme),
                AgentStatus::Unknown,
                &m,
                theme,
            )
        } else {
            slot(label, glyph, status, &m, theme)
        };
        let counts = if state.selected {
            (theme.palette[2], theme.palette[1])
        } else {
            (theme.muted, theme.muted)
        };
        let line = Line::new(cx.look.content_width(cx.width) - indent, m.gap)
            .fixed(m.icon, slot)
            .fill(
                div()
                    .debug_selector(|| format!("name-{label}"))
                    .text_color(rgb(text_color(state, theme))),
                label,
            )
            .when(dirty, |line| {
                line.fixed(dirty_size, parts::dirty(label, dirty_size, theme))
            })
            .when_some(
                pr.and_then(|pr| parts::pr_counts(label, pr, m.glyph, counts)),
                |line, (width, counts)| line.shrink(width, counts.text_size(px(m.small))),
            )
            .when_some(fold, |line, fold| {
                let width = m.icon * 0.6;
                line.fixed(width, fold.element(theme).w(px(width)).text_size(px(14.)))
            });
        shell(label, state, indent, line, cx)
    }

    fn agent(&self, agent: AgentRow<'_>, state: RowState, cx: &RowContext<'_>) -> Div {
        let theme = cx.theme;
        let m = Metrics::new(cx);
        let key = agent.key.as_str();
        let color = text_color(state, theme);
        let glyph = parts::icon(agent.icon.path(), m.icon * 0.7, color);
        // Where the agent runs trails its name, never over half the row.
        let line = Line::new(cx.look.content_width(cx.width), m.gap)
            .fixed(m.icon, slot(key, glyph, agent.status, &m, theme))
            .fill(
                div()
                    .debug_selector(|| format!("name-{key}"))
                    .text_color(rgb(color)),
                agent.name,
            )
            .when_some(parts::agent_place(agent.place, cx.host), |line, place| {
                line.label(
                    div()
                        .debug_selector(|| format!("detail-{key}"))
                        .text_size(px(m.small))
                        .text_color(rgb(theme.muted)),
                    place,
                    m.glyph,
                    0.5,
                )
            })
            .when_some(agent.status_text, |line, text| {
                line.label(
                    div()
                        .debug_selector(|| format!("status-{key}"))
                        .text_size(px(m.small))
                        .text_color(rgb(status_style(agent.status).2)),
                    text,
                    m.glyph,
                    0.5,
                )
            });
        shell(key, state, 0., line, cx)
    }
}
