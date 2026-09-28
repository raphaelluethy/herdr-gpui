//! Text on its way to an agent running in a Herdr pane: the notes written on
//! a page and the comments of a review both travel this road. A delivery waits
//! for its agent to be idle, is pasted into the pane and submitted, and falls
//! back to somewhere the user can still reach it when the pane cannot take it.
//! Nothing here is ever typed into a plain shell, where Enter would run the
//! text, nor into an agent that is asking the user a question.
use crate::{
    HerdrWindow,
    browser::{Batch, Feedback},
    connection::ConnectionBridge,
    terminal::InputTarget,
    window::Flash,
};
use gpui::{ClipboardItem, Context};
use herdr_client::protocol::{
    AgentStatus, ClientKeyCode, ClientKeyKind, ClientPaneInputEvent, ClientShellAgent,
    ClientShellSnapshot,
};
use std::time::{Duration, Instant};

/// How long a delivery waits for a busy agent before it is pasted anyway, or,
/// when the agent is asking a question, handed to its fallback.
pub(crate) const HOLD: Duration = Duration::from_secs(120);
/// Lets the agent's input take the paste before Enter submits it.
const SUBMIT_DELAY: Duration = Duration::from_millis(150);
/// Deliveries waiting at once. Each is one user gesture, so the bound is
/// never reached in practice; it only keeps a wedged agent from hoarding text.
const MAX_DELIVERIES: usize = 32;

/// Where text goes when the agent's pane cannot take it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// Kept for `browser feedback`, which the agent that opened a page fetches.
    Feedback,
    /// Put on the clipboard for the user to paste themselves.
    Clipboard,
}

/// Text waiting for an agent's pane to be ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    pub pane_id: String,
    /// The daemon boot the pane was seen in; a restarted daemon reuses no pane.
    pub boot_id: String,
    pub text: String,
    pub until: Instant,
    pub fallback: Fallback,
    /// What the text is called in the flash that reports on it: "notes" or
    /// "review".
    pub label: &'static str,
}

impl Delivery {
    pub(crate) fn new(
        pane_id: String,
        boot_id: String,
        text: String,
        fallback: Fallback,
        label: &'static str,
        now: Instant,
    ) -> Self {
        Self {
            pane_id,
            boot_id,
            text,
            until: now + HOLD,
            fallback,
            label,
        }
    }
}

/// The window's deliveries in flight, drained every tick by
/// [`HerdrWindow::poll_deliveries`].
#[derive(Default)]
pub(crate) struct Deliveries(Vec<Delivery>);

impl Deliveries {
    /// Queues a delivery. When the bound is reached the oldest gives way and
    /// is reported as dropped; nothing waits forever.
    pub(crate) fn push(&mut self, delivery: Delivery) -> Option<Delivery> {
        let dropped = (self.0.len() >= MAX_DELIVERIES).then(|| self.0.remove(0));
        self.0.push(delivery);
        dropped
    }

    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The agent in `pane_id` and what it is doing, if Herdr sees one there.
pub(crate) fn agent<'a>(
    snapshot: &'a ClientShellSnapshot,
    pane_id: &str,
) -> Option<&'a ClientShellAgent> {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
}

/// What Herdr knows about the pane a delivery is headed for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// The pane is gone, the daemon restarted, or this window shows another.
    Absent,
    /// The pane exists but runs no agent: a shell, where Enter would run the text.
    Shell,
    Agent(AgentStatus),
}

impl Readiness {
    /// Whether text can be queued for this pane at all. A busy agent is fine:
    /// the delivery waits for it.
    pub(crate) fn accepts(self) -> bool {
        matches!(
            self,
            Self::Agent(
                AgentStatus::Idle | AgentStatus::Done | AgentStatus::Working | AgentStatus::Blocked
            )
        )
    }

    pub(crate) fn busy(self) -> bool {
        matches!(
            self,
            Self::Agent(AgentStatus::Working | AgentStatus::Blocked)
        )
    }
}

pub(crate) fn readiness(
    snapshot: Option<&ClientShellSnapshot>,
    boot_id: &str,
    pane_id: &str,
) -> Readiness {
    let Some(snapshot) = snapshot.filter(|snapshot| {
        snapshot.boot_id == boot_id && snapshot.panes.iter().any(|pane| pane.pane_id == pane_id)
    }) else {
        return Readiness::Absent;
    };
    agent(snapshot, pane_id).map_or(Readiness::Shell, |agent| {
        Readiness::Agent(agent.agent_status)
    })
}

/// What a tick does with one delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The agent is busy and the hold has not run out.
    Hold,
    Paste,
    /// The pane cannot take the text; its fallback does.
    FallBack,
}

/// The one decision notes and reviews share. `waiting` is an agent blocked in
/// `browser feedback --wait`, which takes the text directly; `expired` is a
/// hold that ran out, after which a working agent is pasted into anyway while
/// an agent still asking a question is not.
pub(crate) fn verdict(readiness: Readiness, waiting: bool, expired: bool) -> Verdict {
    if waiting {
        return Verdict::FallBack;
    }
    match readiness {
        Readiness::Agent(AgentStatus::Working | AgentStatus::Blocked) if !expired => Verdict::Hold,
        Readiness::Agent(AgentStatus::Idle | AgentStatus::Done | AgentStatus::Working) => {
            Verdict::Paste
        }
        _ => Verdict::FallBack,
    }
}

/// Inline code that survives backticks in the text.
pub(crate) fn code(text: &str) -> String {
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

/// A fence longer than any backtick run in the text.
pub(crate) fn fence(text: &str) -> String {
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    "`".repeat(longest.max(2) + 1)
}

fn enter() -> ClientPaneInputEvent {
    ClientPaneInputEvent::Key {
        code: ClientKeyCode::Enter,
        modifiers: 0,
        kind: ClientKeyKind::Press,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: false,
        physical_key_id: None,
        windows_record: None,
    }
}

impl HerdrWindow {
    /// What this window knows about `pane_id` on the daemon it shows.
    pub(crate) fn pane_readiness(&self, pane_id: &str) -> Readiness {
        let Some(snapshot) = self.live.snapshot.as_deref() else {
            return Readiness::Absent;
        };
        readiness(Some(snapshot), &snapshot.boot_id, pane_id)
    }

    /// Queues `delivery`, reporting a delivery it displaced.
    pub(crate) fn queue_delivery(&mut self, delivery: Delivery, cx: &mut Context<Self>) {
        if let Some(dropped) = self.deliveries.push(delivery) {
            self.fall_back(dropped, Some("Too many deliveries are waiting"), cx);
        }
    }

    /// Pastes held text into agents that became idle. Runs every tick.
    pub(crate) fn poll_deliveries(&mut self, cx: &mut Context<Self>) {
        if self.deliveries.is_empty() {
            return;
        }
        let now = Instant::now();
        let deliveries = std::mem::take(&mut self.deliveries.0);
        for delivery in deliveries {
            let waiting = delivery.fallback == Fallback::Feedback
                && cx
                    .try_global::<Feedback>()
                    .is_some_and(|feedback| feedback.is_waiting(&delivery.pane_id));
            let readiness = readiness(
                self.live.snapshot.as_deref(),
                &delivery.boot_id,
                &delivery.pane_id,
            );
            match verdict(readiness, waiting, now >= delivery.until) {
                Verdict::Hold => self.deliveries.0.push(delivery),
                Verdict::Paste => self.paste_into_pane(delivery, cx),
                // An agent waiting in `browser feedback` is not a failure, so
                // there is nothing to report.
                Verdict::FallBack => {
                    let reason = (!waiting).then_some("The agent is not ready");
                    self.fall_back(delivery, reason, cx);
                }
            }
        }
    }

    /// Hands a delivery to its fallback. `reason`, when given, is what the
    /// flash starts with.
    pub(crate) fn fall_back(
        &mut self,
        delivery: Delivery,
        reason: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let label = delivery.label;
        let kept = match delivery.fallback {
            Fallback::Feedback => {
                cx.default_global::<Feedback>().keep(Batch {
                    pane_id: delivery.pane_id,
                    text: delivery.text,
                });
                format!("{label} kept for `browser feedback`")
            }
            Fallback::Clipboard => {
                cx.write_to_clipboard(ClipboardItem::new_string(delivery.text));
                format!("{label} copied to the clipboard instead")
            }
        };
        if let Some(reason) = reason {
            self.show_flash(Flash::warning(format!("{reason}; {kept}")), cx);
        }
    }

    fn paste_into_pane(&mut self, delivery: Delivery, cx: &mut Context<Self>) {
        let target = InputTarget::Pane(delivery.pane_id.clone());
        let pasted = self.endpoints[self.selected_endpoint]
            .connection
            .handle
            .as_ref()
            .ok_or(crate::Error::NotConnected)
            .and_then(|handle| {
                ConnectionBridge::send_input(
                    handle,
                    &delivery.boot_id,
                    &target,
                    ClientPaneInputEvent::Paste(delivery.text.clone()),
                )
                .map_err(crate::Error::from)
            });
        if let Err(error) = pasted {
            tracing::warn!(%error, label = delivery.label, "Could not paste into the agent's pane");
            self.fall_back(delivery, Some("Could not reach the agent"), cx);
            return;
        }
        let timer = cx.background_executor().clone();
        let boot_id = delivery.boot_id;
        let label = delivery.label;
        cx.spawn(async move |this, cx| {
            timer.timer(SUBMIT_DELAY).await;
            this.update(cx, |this, _| {
                let handle = this.endpoints[this.selected_endpoint]
                    .connection
                    .handle
                    .as_ref();
                if let Some(handle) = handle
                    && let Err(error) =
                        ConnectionBridge::send_input(handle, &boot_id, &target, enter())
                {
                    tracing::warn!(%error, label, "Could not submit the paste in the agent's pane");
                }
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::sidebar::layout_tests::{fixture_window, snapshot};
    use std::sync::Arc;

    fn snapshot_with(pane: &str, agent: Option<&str>) -> ClientShellSnapshot {
        let mut shown: serde_json::Value = serde_json::to_value(snapshot(2)).unwrap();
        shown["panes"] = serde_json::json!([{
            "pane_id": pane, "workspace_id": "w0", "tab_id": "t0", "label": null,
            "cwd": null, "foreground_cwd": null, "focused": true,
            "right_click_passthrough": false
        }]);
        shown["agents"] = serde_json::json!(
            agent
                .map(|status| {
                    vec![serde_json::json!({
                        "pane_id": pane, "workspace_id": "w0", "tab_id": "t0", "name": "claude",
                        "display_agent": "Claude Code", "agent": "claude", "title": null,
                        "terminal_title": null, "terminal_title_stripped": null,
                        "agent_status": status, "state_change_seq": 0, "state_labels": [],
                        "tokens": [], "focused": true
                    })]
                })
                .unwrap_or_default()
        );
        serde_json::from_value(shown).unwrap()
    }

    #[test]
    fn readiness_tells_a_shell_from_an_agent_and_another_boot_from_this_one() {
        let shown = snapshot_with("w0:p1", Some("working"));
        assert_eq!(
            readiness(Some(&shown), &shown.boot_id, "w0:p1"),
            Readiness::Agent(AgentStatus::Working)
        );
        assert_eq!(
            readiness(Some(&shown), "another-boot", "w0:p1"),
            Readiness::Absent
        );
        assert_eq!(
            readiness(Some(&shown), &shown.boot_id, "w0:p2"),
            Readiness::Absent
        );
        assert_eq!(readiness(None, &shown.boot_id, "w0:p1"), Readiness::Absent);
        let shell = snapshot_with("w0:p1", None);
        assert_eq!(
            readiness(Some(&shell), &shell.boot_id, "w0:p1"),
            Readiness::Shell
        );
        assert!(!Readiness::Shell.accepts() && !Readiness::Absent.accepts());
        assert!(Readiness::Agent(AgentStatus::Blocked).accepts());
        assert!(Readiness::Agent(AgentStatus::Blocked).busy());
        assert!(!Readiness::Agent(AgentStatus::Unknown).accepts());
    }

    #[test]
    fn only_a_running_agents_prompt_is_typed_into() {
        use AgentStatus::*;
        let agent = Readiness::Agent;
        // Busy agents wait for the hold; only a working one is pasted into once
        // it runs out, because an agent asking a question must not have its
        // answer typed by a paste.
        assert_eq!(verdict(agent(Working), false, false), Verdict::Hold);
        assert_eq!(verdict(agent(Blocked), false, false), Verdict::Hold);
        assert_eq!(verdict(agent(Working), false, true), Verdict::Paste);
        assert_eq!(verdict(agent(Blocked), false, true), Verdict::FallBack);
        assert_eq!(verdict(agent(Idle), false, false), Verdict::Paste);
        assert_eq!(verdict(agent(Done), false, false), Verdict::Paste);
        assert_eq!(verdict(agent(Unknown), false, false), Verdict::FallBack);
        // A shell or a missing pane never receives a paste.
        assert_eq!(verdict(Readiness::Shell, false, true), Verdict::FallBack);
        assert_eq!(verdict(Readiness::Absent, false, false), Verdict::FallBack);
        // An agent waiting in `browser feedback` is handed the text there.
        assert_eq!(verdict(agent(Idle), true, false), Verdict::FallBack);
    }

    #[test]
    fn quoting_outgrows_the_texts_own_backticks() {
        assert_eq!(code("a"), "`a`");
        assert_eq!(code("a`b"), "`` a`b ``");
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("has ``` inside"), "````");
        assert_eq!(fence("`````"), "``````");
    }

    #[test]
    fn the_queue_is_bounded_and_reports_what_it_displaces() {
        let mut deliveries = Deliveries::default();
        let now = Instant::now();
        let delivery = |index: usize| {
            Delivery::new(
                "p".into(),
                "b".into(),
                index.to_string(),
                Fallback::Clipboard,
                "review",
                now,
            )
        };
        for index in 0..MAX_DELIVERIES {
            assert!(deliveries.push(delivery(index)).is_none());
        }
        let dropped = deliveries.push(delivery(MAX_DELIVERIES)).unwrap();
        assert_eq!(dropped.text, "0");
        assert_eq!(deliveries.len(), MAX_DELIVERIES);
        assert_eq!(dropped.until, now + HOLD);
    }

    #[gpui::test]
    fn a_clipboard_delivery_falls_back_to_the_clipboard_when_its_pane_goes(
        cx: &mut gpui::TestAppContext,
    ) {
        let (view, cx) = cx.add_window_view(fixture_window);
        let shown = snapshot_with("w0:p1", Some("working"));
        let boot = shown.boot_id.clone();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.live.snapshot = Some(Arc::new(shown));
                view.queue_delivery(
                    Delivery::new(
                        "w0:p1".into(),
                        boot.clone(),
                        "Review text".into(),
                        Fallback::Clipboard,
                        "review",
                        Instant::now(),
                    ),
                    cx,
                );
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 1, "held while the agent works");
                assert!(cx.read_from_clipboard().is_none());
                // The daemon restarted: the pane id means nothing any more.
                let mut restarted = (*view.live.snapshot.clone().unwrap()).clone();
                restarted.boot_id = "rebooted".into();
                view.live.snapshot = Some(Arc::new(restarted));
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 0);
                assert_eq!(
                    cx.read_from_clipboard().and_then(|item| item.text()),
                    Some("Review text".into())
                );
                assert!(
                    view.flash
                        .as_ref()
                        .is_some_and(|(flash, _)| flash.text.contains("copied to the clipboard"))
                );
            })
        });
    }

    #[gpui::test]
    fn an_idle_agent_without_a_connection_falls_back_at_once(cx: &mut gpui::TestAppContext) {
        let (view, cx) = cx.add_window_view(fixture_window);
        let shown = snapshot_with("w0:p1", Some("idle"));
        let boot = shown.boot_id.clone();
        cx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.live.snapshot = Some(Arc::new(shown));
                view.queue_delivery(
                    Delivery::new(
                        "w0:p1".into(),
                        boot,
                        "Notes".into(),
                        Fallback::Feedback,
                        "notes",
                        Instant::now(),
                    ),
                    cx,
                );
                // This fixture has no connection, so the paste fails and the
                // notes are kept for `browser feedback` rather than lost.
                view.poll_deliveries(cx);
                assert_eq!(view.deliveries.len(), 0);
                assert_eq!(
                    cx.default_global::<Feedback>().take("w0:p1").as_deref(),
                    Some("Notes")
                );
                assert!(
                    view.flash
                        .as_ref()
                        .is_some_and(|(flash, _)| flash.text.contains("browser feedback"))
                );
            })
        });
    }
}
