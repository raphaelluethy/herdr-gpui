//! A Git remote URL, parsed into the forge, host, and project path it names.
//! Remote text is untrusted: anything that does not match a known shape
//! exactly is refused rather than guessed at.

use super::Kind;

/// A repository on a supported forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Remote {
    pub kind: Kind,
    /// The forge's host as its API is addressed.
    pub host: String,
    /// `owner/repo` on GitHub.
    pub path: String,
}

impl Remote {
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let (owner, repo) = crate::avatars::github_repo(url)?;
        Some(Self {
            kind: Kind::GitHub,
            host: "github.com".into(),
            path: format!("{owner}/{repo}"),
        })
    }

    #[cfg(test)]
    pub(crate) fn github(owner: &str, repo: &str) -> Self {
        Self {
            kind: Kind::GitHub,
            host: "github.com".into(),
            path: format!("{owner}/{repo}"),
        }
    }

    /// Everything before the repository name: a GitHub owner.
    pub(crate) fn owner(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(owner, _)| owner)
    }

    /// The repository's own name, the path's last segment.
    pub(crate) fn name(&self) -> &str {
        self.path
            .rsplit_once('/')
            .map_or(self.path.as_str(), |(_, name)| name)
    }

    /// The repository as users write it, such as `owner/repo`.
    pub(crate) fn slug(&self) -> &str {
        &self.path
    }
}
