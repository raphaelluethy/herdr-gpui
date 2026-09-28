//! A branch's merge request on GitLab, read through the user's `glab` and
//! presented as a `PullRequest`. Replies are untrusted: a merge request must
//! belong to the project and source that were asked for, its text is cleaned,
//! and its link is rebuilt from the verified project address.

use super::{
    PullRequest, Result, State, clean,
    fetch::Head,
    model::{Check, MergeState, Outcome, Owner, ReviewDecision},
};
use crate::{
    Error,
    forge::{
        Kind, Remote,
        gitlab::{Client, Project, encode},
    },
};
use serde_json::Value;
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Merge requests read for one branch. Without a configured upstream only
/// two are asked for, as on GitHub: two for the same branch is ambiguous.
const FORK_LIMIT: u64 = 100;
const LIMIT: u64 = 2;

pub(super) fn lookup(
    program: &Path,
    remote: &Remote,
    branch: &str,
    head: Option<&Head>,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> Result {
    let client = Client {
        program,
        host: &remote.host,
    };
    let project = client.project(remote, deadline, cancelled, cooldown)?;
    // A branch pushed to a fork is found by the fork's project ID. An upstream
    // on another host or forge can never be this project's merge request.
    let source = match head {
        Some(head) if head.remote.kind != Kind::GitLab || head.remote.host != remote.host => {
            return Ok(None);
        }
        Some(head) if !head.remote.path.eq_ignore_ascii_case(&remote.path) => {
            client
                .project(&head.remote, deadline, cancelled, cooldown)?
                .id
        }
        _ => project.id,
    };
    let limit = if head.is_some() { FORK_LIMIT } else { LIMIT };
    let list = client.get(
        "gitlab_merge_requests",
        &format!(
            "projects/{}/merge_requests?source_branch={}&order_by=updated_at&sort=desc&per_page={limit}",
            project.id,
            encode(branch)
        ),
        deadline,
        cancelled,
        cooldown,
    )?;
    let Some(iid) = select(&list, project.id, source, branch)? else {
        return Ok(None);
    };
    // Only the single-merge-request reply carries the head pipeline.
    let detail = client.get(
        "gitlab_merge_request",
        &format!("projects/{}/merge_requests/{iid}", project.id),
        deadline,
        cancelled,
        cooldown,
    )?;
    merge_request(&detail, &project, source, branch).map(Some)
}

/// The one merge request from `source`'s `branch` into this project.
pub(super) fn select(
    list: &Value,
    project: u64,
    source: u64,
    branch: &str,
) -> crate::Result<Option<u64>> {
    let entries = list.as_array().ok_or(Error::GitLabProject)?;
    let mut matching = entries
        .iter()
        .filter(|entry| {
            entry["source_branch"].as_str() == Some(branch)
                && entry["source_project_id"].as_u64() == Some(source)
                && entry["target_project_id"].as_u64() == Some(project)
        })
        .filter_map(|entry| entry["iid"].as_u64().filter(|iid| *iid > 0));
    let first = matching.next();
    if matching.next().is_some() {
        return Err(Error::PrAmbiguous);
    }
    Ok(first)
}

/// One merge request reply as this client's pull request model.
pub(super) fn merge_request(
    value: &Value,
    project: &Project,
    source: u64,
    branch: &str,
) -> crate::Result<PullRequest> {
    let iid = value["iid"].as_u64().filter(|iid| *iid > 0);
    let state = match value["state"].as_str() {
        // `locked` is the brief state while GitLab performs a merge.
        Some("opened" | "locked") => State::Open,
        Some("closed") => State::Closed,
        Some("merged") => State::Merged,
        _ => State::Unknown,
    };
    let Some(iid) = iid.filter(|_| {
        state != State::Unknown
            && value["source_branch"].as_str() == Some(branch)
            && value["source_project_id"].as_u64() == Some(source)
            && value["target_project_id"].as_u64() == Some(project.id)
    }) else {
        return Err(Error::PrIdentity);
    };
    let detailed = value["detailed_merge_status"].as_str().unwrap_or_default();
    let outcome = value["head_pipeline"]["status"].as_str().map(pipeline);
    let text = |key: &str| clean(value[key].as_str().unwrap_or_default());
    Ok(PullRequest {
        number: iid,
        url: format!("{}/-/merge_requests/{iid}", project.web_url),
        title: text("title"),
        state,
        is_draft: value["draft"] == true || value["work_in_progress"] == true,
        head_ref_name: clean(branch),
        base_ref_name: text("target_branch"),
        // GitLab's merge request API reports no line counts; `line_counts`
        // says so rather than showing these zeros.
        additions: 0,
        deletions: 0,
        changed_files: 0,
        updated_at: text("updated_at"),
        merge_state_status: merge_state(detailed, value["has_conflicts"] == true),
        review_decision: match detailed {
            "not_approved" => ReviewDecision::ReviewRequired,
            "requested_changes" => ReviewDecision::ChangesRequested,
            _ => ReviewDecision::None,
        },
        checks_summary: match outcome {
            Some(outcome) => format!("Pipeline {outcome}"),
            None => "No pipeline reported".into(),
        },
        forge: Kind::GitLab,
        status_check_rollup: outcome.map(|outcome| vec![Check::pipeline(outcome)]),
        head_repository_owner: Owner {
            login: String::new(),
        },
    })
}

/// A head pipeline's status as a check outcome.
pub(super) fn pipeline(status: &str) -> Outcome {
    match status {
        "success" => Outcome::Passed,
        "failed" | "canceled" | "canceling" => Outcome::Failed,
        "skipped" => Outcome::Skipped,
        _ => Outcome::Pending,
    }
}

/// GitLab's `detailed_merge_status`, in the terms GitHub's merge state uses.
/// Unknown values degrade to "unavailable", as on GitHub.
pub(super) fn merge_state(detailed: &str, conflicts: bool) -> MergeState {
    if conflicts {
        return MergeState::Dirty;
    }
    match detailed {
        "mergeable" => MergeState::Clean,
        "conflict" | "broken_status" => MergeState::Dirty,
        "need_rebase" => MergeState::Behind,
        "draft_status" => MergeState::Draft,
        "ci_must_pass"
        | "ci_still_running"
        | "discussions_not_resolved"
        | "not_approved"
        | "requested_changes"
        | "blocked_status"
        | "external_status_checks"
        | "status_checks_must_pass"
        | "jira_association_missing"
        | "merge_request_blocked"
        | "security_policy_violations"
        | "merge_time"
        | "locked_paths"
        | "locked_lfs_files"
        | "title_regex" => MergeState::Blocked,
        _ => MergeState::Unknown,
    }
}
