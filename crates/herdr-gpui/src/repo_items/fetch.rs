//! Listing a repository's recent pull requests and issues, and making a pull
//! request's head branch reachable before a checkout is asked for. Every step
//! has a deadline and runs off the UI thread.

use super::{
    Item, Kind,
    model::{LIMIT, parse, parse_gitlab},
};
use crate::{
    Error,
    forge::{Access, Forges, Remote, Variable, gitlab::Client},
    pull_request::{Input, local_checkout, local_repository, run},
};
use std::path::Path;
use std::{
    process::Command,
    time::{Duration, Instant},
};

pub(super) const TIMEOUT: Duration = Duration::from_secs(20);
/// Fetching one ref can wait on the network and on credential prompts that are
/// disabled for these children, so it gets its own, longer budget.
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

const QUERY: &str = r#"query($owner: String!, $repo: String!, $count: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequests(first: $count, states: OPEN, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes { number title isDraft headRefName author { login } headRepositoryOwner { login } }
    }
    issues(first: $count, states: OPEN, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes { number title author { login } }
    }
  }
}"#;

/// The repository's open pull requests (merge requests, on GitLab) and
/// issues, most recently updated first.
pub(super) fn list(
    input: &Input,
    forges: &Forges,
    cancelled: impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> crate::Result<(Remote, Vec<Item>)> {
    let deadline = Instant::now() + TIMEOUT;
    let origin = local_repository(input, forges, deadline, &cancelled)?;
    let access = forges.access(&origin)?;
    if let Access::Glab(program) = access {
        let items = gitlab(program, &origin, deadline, &cancelled, cooldown)?;
        return Ok((origin, items));
    }
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::PrTimeout)?;
    let response = crate::forge::graphql(
        "repo_items",
        access,
        QUERY,
        &[
            ("owner", Variable::Text(origin.owner())),
            ("repo", Variable::Text(origin.name())),
            ("count", Variable::Int(LIMIT as u64)),
        ],
        timeout,
        cancelled,
        cooldown,
    )?;
    let mut items = parse(&response, &origin, Kind::PullRequest)?;
    items.extend(parse(&response, &origin, Kind::Issue)?);
    Ok((origin, items))
}

fn gitlab(
    program: &Path,
    origin: &Remote,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> crate::Result<Vec<Item>> {
    let client = Client {
        program,
        host: &origin.host,
    };
    let project = client.project(origin, deadline, cancelled, cooldown)?;
    let mut items = Vec::new();
    for (kind, context, path) in [
        (Kind::PullRequest, "gitlab_merge_requests", "merge_requests"),
        (Kind::Issue, "gitlab_issues", "issues"),
    ] {
        let listing = client.get(
            context,
            &format!(
                "projects/{}/{path}?state=opened&order_by=updated_at&sort=desc&per_page={LIMIT}",
                project.id
            ),
            deadline,
            cancelled,
            cooldown,
        )?;
        items.extend(parse_gitlab(&listing, &project, kind)?);
    }
    Ok(items)
}

/// Fetch the PR's head from origin, including GitHub's published fork PR refs
/// and GitLab's merge request refs.
/// Existing local branches are never moved by this fetch.
pub(super) fn fetch_branch(
    input: &Input,
    item: &Item,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<()> {
    let deadline = Instant::now() + FETCH_TIMEOUT;
    let checkout = local_checkout(input, deadline, cancelled)?;
    let refspec = item.fetch_refspec();
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "-C", &checkout])
        .args(["fetch", "--no-tags", "--quiet", "origin", &refspec]);
    let (ok, output) = run(&mut command, deadline, cancelled)?;
    if ok {
        return Ok(());
    }
    Err(Error::GitFailed {
        operation: "fetch the pull request branch",
        details: crate::pull_request::clean(output.trim()),
    })
}
