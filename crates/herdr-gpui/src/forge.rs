//! Which forge a checkout's origin belongs to, and how this client may reach
//! it on the user's behalf: the native GitHub sign-in, or the user's own
//! authenticated `gh` or `glab` CLI. A CLI's credential never enters this
//! process; its requests run as bounded child processes under the policy in
//! `cli`.

mod cli;
#[cfg(all(test, unix))]
pub(crate) mod fake;
mod github;
pub(crate) mod gitlab;
mod probe;
mod remote;

#[cfg(test)]
mod tests;

pub(crate) use {
    github::{Variable, graphql},
    probe::{Account, Enabled, Probe, Status},
    remote::Remote,
};

use crate::{Error, Result, config::GitHubCli};
use secrecy::SecretString;
use std::{path::Path, sync::Arc};

/// A code forge this client knows how to talk to. Public only because
/// `Error` variants carry it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    GitHub,
    GitLab,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
        }
    }

    /// The command-line tool that can stand in for a native sign-in.
    pub(crate) fn cli(self) -> &'static str {
        match self {
            Self::GitHub => "gh",
            Self::GitLab => "glab",
        }
    }

    /// How the user signs the CLI in, for messages that ask them to.
    pub(crate) fn login_hint(self) -> &'static str {
        match self {
            Self::GitHub => "Run `gh auth login`, or sign in from the GitHub panel.",
            Self::GitLab => "Run `glab auth login` for this host.",
        }
    }

    /// What the forge calls a request to merge a branch, in full.
    pub(crate) fn change_noun(self) -> &'static str {
        match self {
            Self::GitHub => "pull request",
            Self::GitLab => "merge request",
        }
    }

    /// The same, abbreviated as each forge's own UI does.
    pub(crate) fn change_abbreviation(self) -> &'static str {
        match self {
            Self::GitHub => "PR",
            Self::GitLab => "MR",
        }
    }

    /// How the forge writes a change's number: `#8` on GitHub, `!8` on GitLab.
    pub(crate) fn change_reference(self, number: u64) -> String {
        match self {
            Self::GitHub => format!("#{number}"),
            Self::GitLab => format!("!{number}"),
        }
    }
}

/// How requests to a forge are authorized.
#[derive(Clone, Debug)]
pub(crate) enum Access {
    /// The native device-flow sign-in's token.
    Native(Arc<SecretString>),
    /// The user's authenticated GitHub CLI, at this resolved path.
    Gh(Arc<Path>),
    /// The user's authenticated GitLab CLI, at this resolved path.
    Glab(Arc<Path>),
}

/// Two grants are equal when they are the same grant: the same token
/// allocation (its identity, never its text) or the same CLI executable.
impl PartialEq for Access {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Native(left), Self::Native(right)) => Arc::ptr_eq(left, right),
            (Self::Gh(left), Self::Gh(right)) | (Self::Glab(left), Self::Glab(right)) => {
                left == right
            }
            _ => false,
        }
    }
}

impl Eq for Access {}

/// Every grant this window currently holds, one per forge. Workers receive a
/// copy and pick the grant that matches the origin they resolve, so the UI
/// never has to know a checkout's forge before a lookup has run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Forges {
    pub github: Option<Access>,
    pub gitlab: Option<Access>,
    /// The hosts `glab` reports as signed in. These and gitlab.com are the
    /// remotes recognized as GitLab, and only these can be reached.
    pub gitlab_hosts: Arc<[String]>,
}

impl Forges {
    pub(crate) fn is_empty(&self) -> bool {
        self.github.is_none() && self.gitlab.is_none()
    }

    /// Parse a Git remote as one of the forges this client supports.
    pub(crate) fn remote(&self, url: &str) -> Option<Remote> {
        Remote::parse(url, &self.gitlab_hosts)
    }

    /// The grant for `remote`'s forge, or why there is none.
    pub(crate) fn access(&self, remote: &Remote) -> Result<&Access> {
        match remote.kind {
            Kind::GitHub => self.github.as_ref(),
            Kind::GitLab => self.gitlab.as_ref().filter(|_| {
                self.gitlab_hosts
                    .iter()
                    .any(|host| host.eq_ignore_ascii_case(&remote.host))
            }),
        }
        .ok_or(Error::ForgeAccess(remote.kind))
    }
}

/// Which GitHub grant is in effect. The native sign-in wins unless the config
/// prefers the CLI; `off` never uses the CLI.
pub(crate) fn github_access(
    native: Option<&Arc<SecretString>>,
    gh: Option<&Arc<Path>>,
    mode: GitHubCli,
) -> Option<Access> {
    let native = || native.cloned().map(Access::Native);
    let gh = || gh.cloned().map(Access::Gh);
    match mode {
        GitHubCli::Off => native(),
        GitHubCli::Auto => native().or_else(gh),
        GitHubCli::Prefer => gh().or_else(native),
    }
}
