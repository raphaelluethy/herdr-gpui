//! Recent pull requests and issues for one repository, and the branch each one
//! seeds. Remote text is untrusted: titles and branch names are cleaned and
//! length-bounded before they can reach a label or a daemon request.

use crate::{
    Error,
    forge::{self, Remote},
    pull_request::clean,
};
use serde_json::Value;

/// Items per list. The tabs are a picker, not a mirror of the repository, so a
/// query stays inside one page and one bounded response.
pub(super) const LIMIT: usize = 50;
/// Issue branches follow GitHub's own "create a branch" naming. The slug is
/// bounded so a long title cannot produce an unusable checkout directory.
const SLUG_LIMIT: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    PullRequest,
    Issue,
}

impl Kind {
    /// "PR" on GitHub, "MR" on GitLab.
    pub(crate) fn tab_label(self, forge: forge::Kind) -> &'static str {
        match self {
            Self::PullRequest => forge.change_abbreviation(),
            Self::Issue => "issues",
        }
    }

    pub(crate) fn empty_label(self, forge: forge::Kind) -> &'static str {
        match (self, forge) {
            (Self::PullRequest, forge::Kind::GitHub) => "No open pull requests match.",
            (Self::PullRequest, forge::Kind::GitLab) => "No open merge requests match.",
            (Self::Issue, _) => "No open issues match.",
        }
    }

    /// What the listing is, for the status line under the tabs.
    pub(crate) fn listing(self, forge: forge::Kind) -> &'static str {
        match (self, forge) {
            (Self::PullRequest, forge::Kind::GitHub) => "open pull requests",
            (Self::PullRequest, forge::Kind::GitLab) => "open merge requests",
            (Self::Issue, _) => "open issues",
        }
    }

    fn field(self) -> &'static str {
        match self {
            Self::PullRequest => "pullRequests",
            Self::Issue => "issues",
        }
    }
}

/// One listed pull request (merge request, on GitLab) or issue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Item {
    pub forge: forge::Kind,
    pub kind: Kind,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    /// A pull request's head branch. An issue has none until one is created.
    pub head: Option<String>,
    /// Set when the head branch lives in a fork rather than the repository.
    pub fork: bool,
    pub draft: bool,
}

impl Item {
    /// The branch a new checkout for this item uses: a pull request's existing
    /// head branch, or the branch an issue's number and title name.
    pub(crate) fn branch(&self) -> String {
        match &self.head {
            Some(_) if self.fork => match self.forge {
                forge::Kind::GitHub => format!("pr/{}", self.number),
                forge::Kind::GitLab => format!("mr/{}", self.number),
            },
            Some(head) => head.clone(),
            None => issue_branch(self.number, &self.title),
        }
    }

    /// Fork heads use the forge's own change ref (GitHub's `refs/pull/N/head`,
    /// GitLab's `refs/merge-requests/N/head`), isolated from origin's branch
    /// namespace.
    pub(crate) fn base_ref(&self) -> String {
        match (self.fork, self.forge) {
            (true, forge::Kind::GitHub) => format!("refs/herdr/pull/{}/head", self.number),
            (true, forge::Kind::GitLab) => {
                format!("refs/herdr/merge-requests/{}/head", self.number)
            }
            (false, _) => format!("refs/remotes/origin/{}", self.branch()),
        }
    }

    pub(super) fn fetch_refspec(&self) -> String {
        let source = match (self.fork, self.forge) {
            (true, forge::Kind::GitHub) => format!("refs/pull/{}/head", self.number),
            (true, forge::Kind::GitLab) => format!("refs/merge-requests/{}/head", self.number),
            (false, _) => format!("refs/heads/{}", self.branch()),
        };
        format!("+{source}:{}", self.base_ref())
    }

    /// `#8`, or `!8` for a GitLab merge request; issues are `#8` on both.
    pub(crate) fn reference(&self) -> String {
        match self.kind {
            Kind::PullRequest => self.forge.change_reference(self.number),
            Kind::Issue => format!("#{}", self.number),
        }
    }

    /// What the picker's search matches against, lowercased once per item.
    pub(crate) fn search_key(&self) -> String {
        format!("{} {} {}", self.reference(), self.title, self.author).to_lowercase()
    }

    pub(crate) fn label(&self) -> String {
        format!("{} {}", self.reference(), self.title)
    }
}

/// GitHub's branch name for an issue: the number, then a slug of the title.
pub(crate) fn issue_branch(number: u64, title: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !slug.is_empty() {
            slug.push('-');
            dash = true;
        }
        if slug.len() >= SLUG_LIMIT {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        number.to_string()
    } else {
        format!("{number}-{slug}")
    }
}

/// Read one list out of a GraphQL response. A repository the token cannot see
/// comes back as a null node rather than an error, so absence is rejected here.
pub(super) fn parse(response: &Value, origin: &Remote, kind: Kind) -> crate::Result<Vec<Item>> {
    let nodes = response["data"]["repository"][kind.field()]["nodes"]
        .as_array()
        .ok_or(Error::PrRepository)?;
    Ok(nodes
        .iter()
        .take(LIMIT)
        .filter_map(|node| item(node, origin, kind))
        .collect())
}

fn item(node: &Value, origin: &Remote, kind: Kind) -> Option<Item> {
    let number = node["number"].as_u64().filter(|number| *number > 0)?;
    // A pull request without a usable head ref cannot seed a checkout, so it is
    // dropped rather than listed as a row that can only fail.
    let head = match kind {
        Kind::PullRequest => Some(branch_name(node["headRefName"].as_str()?)?),
        Kind::Issue => None,
    };
    let fork = head.is_some()
        && !node["headRepositoryOwner"]["login"]
            .as_str()
            .unwrap_or_default()
            .eq_ignore_ascii_case(origin.owner());
    Some(Item {
        forge: forge::Kind::GitHub,
        kind,
        number,
        title: clean(node["title"].as_str().unwrap_or_default()),
        // Rebuilt from the repository that was asked for, so a response can
        // never point a row at another repository.
        url: format!(
            "https://github.com/{}/{}/{number}",
            origin.slug(),
            match kind {
                Kind::PullRequest => "pull",
                Kind::Issue => "issues",
            }
        ),
        author: clean(node["author"]["login"].as_str().unwrap_or_default()),
        head,
        fork,
        draft: node["isDraft"] == true,
    })
}

/// Read a GitLab list of open merge requests or issues. Links are rebuilt
/// from the verified project address, and a merge request whose source is
/// another project is marked as a fork, fetched by its merge request ref.
pub(super) fn parse_gitlab(
    value: &Value,
    project: &forge::gitlab::Project,
    kind: Kind,
) -> crate::Result<Vec<Item>> {
    let entries = value.as_array().ok_or(Error::GitLabProject)?;
    Ok(entries
        .iter()
        .take(LIMIT)
        .filter_map(|entry| gitlab_item(entry, project, kind))
        .collect())
}

fn gitlab_item(entry: &Value, project: &forge::gitlab::Project, kind: Kind) -> Option<Item> {
    let number = entry["iid"].as_u64().filter(|number| *number > 0)?;
    let (head, fork, path) = match kind {
        Kind::PullRequest => {
            if entry["target_project_id"].as_u64() != Some(project.id) {
                return None;
            }
            (
                Some(branch_name(entry["source_branch"].as_str()?)?),
                entry["source_project_id"].as_u64() != Some(project.id),
                "merge_requests",
            )
        }
        Kind::Issue => (None, false, "issues"),
    };
    Some(Item {
        forge: forge::Kind::GitLab,
        kind,
        number,
        title: clean(entry["title"].as_str().unwrap_or_default()),
        url: format!("{}/-/{path}/{number}", project.web_url),
        author: clean(entry["author"]["username"].as_str().unwrap_or_default()),
        head,
        fork,
        draft: entry["draft"] == true || entry["work_in_progress"] == true,
    })
}

/// A remote branch name this client is willing to put in a daemon request.
/// Git already forbids these shapes, so anything else is a hostile response.
pub(super) fn branch_name(value: &str) -> Option<String> {
    let name = value.trim();
    (!name.is_empty()
        && name.len() <= 255
        && !name.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        && !name.starts_with('-')
        && !name.contains(".."))
    .then(|| name.to_owned())
}
