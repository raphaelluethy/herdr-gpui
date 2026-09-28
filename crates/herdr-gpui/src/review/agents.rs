//! The agents a review can be sent to: every agent Herdr reports, the ones in
//! the reviewed workspace first. Herdr's snapshot is the only source; this
//! client never guesses at what runs in a pane.
use herdr_client::protocol::{AgentStatus, ClientShellSnapshot};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub pane_id: String,
    /// The agent's kind and name, such as "Claude Code · review".
    pub label: String,
    /// Where it runs: its tab and, outside the reviewed workspace, that
    /// workspace too.
    pub detail: String,
    pub status: AgentStatus,
    /// Whether it runs in the reviewed workspace.
    pub local: bool,
}

/// One line for a label, bounded and free of controls.
fn short(text: &str, limit: usize) -> String {
    super::notes::single_line(text, limit)
}

/// The agents to offer, in order: those of `workspace_id`, its focused one
/// first, then every other agent in the daemon's order.
pub(crate) fn candidates(
    snapshot: &ClientShellSnapshot,
    workspace_id: Option<&str>,
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = snapshot
        .agents
        .iter()
        .map(|agent| {
            let local = Some(agent.workspace_id.as_str()) == workspace_id;
            let kind = agent
                .display_agent
                .as_deref()
                .or(agent.agent.as_deref())
                .filter(|kind| !kind.trim().is_empty())
                .unwrap_or("Agent");
            let label = match agent.name.as_deref().filter(|name| !name.trim().is_empty()) {
                Some(name) => format!("{} \u{00b7} {}", short(kind, 40), short(name, 60)),
                None => short(kind, 40),
            };
            let tab = snapshot
                .tabs
                .iter()
                .find(|tab| tab.tab_id == agent.tab_id)
                .map(|tab| short(&tab.label, 40));
            let workspace = (!local)
                .then(|| {
                    snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.workspace_id == agent.workspace_id)
                        .map(|workspace| short(&workspace.label, 40))
                })
                .flatten();
            let detail = [tab, workspace]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" \u{00b7} ");
            Candidate {
                pane_id: agent.pane_id.clone(),
                label,
                detail,
                status: agent.agent_status,
                local,
            }
        })
        .collect();
    let focused = snapshot.focused_pane_id.as_deref();
    // Stable: within each group the daemon's order stands.
    candidates.sort_by_key(|candidate| {
        (
            !candidate.local,
            !(candidate.local && Some(candidate.pane_id.as_str()) == focused),
        )
    });
    candidates
}

/// The candidate chosen by default: the first, which is the focused agent of
/// the reviewed workspace when there is one.
pub(crate) fn default_choice(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates.first()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::sidebar::layout_tests::snapshot;

    fn with_agents(agents: &[(&str, &str, &str, Option<&str>)]) -> ClientShellSnapshot {
        let mut shown: serde_json::Value = serde_json::to_value(snapshot(3)).unwrap();
        shown["agents"] = serde_json::json!(
            agents
                .iter()
                .map(|(pane, workspace, status, name)| {
                    serde_json::json!({
                        "pane_id": pane, "workspace_id": workspace, "tab_id": "t0",
                        "name": name, "display_agent": "Claude Code", "agent": "claude",
                        "title": null, "terminal_title": null, "terminal_title_stripped": null,
                        "agent_status": status, "state_change_seq": 0, "state_labels": [],
                        "tokens": [], "focused": false
                    })
                })
                .collect::<Vec<_>>()
        );
        serde_json::from_value(shown).unwrap()
    }

    #[test]
    fn the_reviewed_workspaces_agents_come_first_and_its_focused_one_leads() {
        let mut shown = with_agents(&[
            ("p-other", "w2", "idle", Some("elsewhere")),
            ("p-a", "w1", "working", Some("a")),
            ("p-b", "w1", "blocked", None),
            ("p-c", "w1", "done", Some("c")),
        ]);
        shown.focused_pane_id = Some("p-b".into());
        let list = candidates(&shown, Some("w1"));
        let panes: Vec<_> = list.iter().map(|c| c.pane_id.as_str()).collect();
        assert_eq!(panes, ["p-b", "p-a", "p-c", "p-other"]);
        assert_eq!(default_choice(&list).unwrap().pane_id, "p-b");
        assert_eq!(list[0].label, "Claude Code");
        assert_eq!(list[1].label, "Claude Code \u{00b7} a");
        assert!(list[0].local && !list[3].local);
        assert_eq!(list[0].detail, "tab 1");
        assert_eq!(list[3].detail, "tab 1 \u{00b7} another workspace");
        assert_eq!(list[2].status, AgentStatus::Done);
        // Without a focused pane in the workspace, the daemon's order stands.
        shown.focused_pane_id = Some("p-other".into());
        let list = candidates(&shown, Some("w1"));
        let panes: Vec<_> = list.iter().map(|c| c.pane_id.as_str()).collect();
        assert_eq!(panes, ["p-a", "p-b", "p-c", "p-other"]);
        // No workspace to prefer: the daemon's order, nothing local.
        let list = candidates(&shown, None);
        assert_eq!(list[0].pane_id, "p-other");
        assert!(list.iter().all(|c| !c.local));
        assert!(default_choice(&[]).is_none());
    }

    #[test]
    fn labels_are_bounded_single_lines() {
        let long = "n".repeat(200);
        let shown = with_agents(&[("p", "w0", "idle", Some(&format!("bad\u{1b}name {long}")))]);
        let list = candidates(&shown, Some("w0"));
        assert!(list[0].label.starts_with("Claude Code \u{00b7} bad name"));
        assert!(list[0].label.chars().count() <= 40 + 3 + 61);
        assert!(!list[0].label.chars().any(char::is_control));
    }
}
