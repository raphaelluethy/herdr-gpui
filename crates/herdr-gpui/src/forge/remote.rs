//! A Git remote URL, parsed into the forge, host, and project path it names.
//! Remote text is untrusted: anything that does not match a known shape
//! exactly is refused rather than guessed at.

use super::Kind;

/// GitLab nests groups at most this deep, so a longer path is not a project.
const MAX_SEGMENTS: usize = 21;
/// A bound on each path segment, above both forges' own name limits.
const MAX_SEGMENT: usize = 255;
const MAX_HOST: usize = 253;

/// A repository on a supported forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Remote {
    pub kind: Kind,
    /// The forge's host as its API is addressed: `github.com`, or for GitLab
    /// the host exactly as `glab` names it, port included when it has one.
    pub host: String,
    /// `owner/repo` on GitHub; `group/subgroup/…/project` on GitLab.
    pub path: String,
}

impl Remote {
    /// GitHub.com remotes keep the strict shape avatars have always used.
    /// GitLab remotes are recognized on gitlab.com and on any host in
    /// `gitlab_hosts`, the hosts the user's `glab` is signed in to.
    pub(crate) fn parse(url: &str, gitlab_hosts: &[String]) -> Option<Self> {
        if let Some((owner, repo)) = crate::avatars::github_repo(url) {
            return Some(Self {
                kind: Kind::GitHub,
                host: "github.com".into(),
                path: format!("{owner}/{repo}"),
            });
        }
        let (host, port, path) = split(url)?;
        if host == "github.com" {
            return None;
        }
        // glab may name a host with its API port; an SSH remote's port is an
        // SSH port and says nothing about which instance it is.
        let host = gitlab_hosts
            .iter()
            .find(|known| {
                known.eq_ignore_ascii_case(&host)
                    || port
                        .is_some_and(|port| known.eq_ignore_ascii_case(&format!("{host}:{port}")))
            })
            .cloned()
            .or_else(|| (host == "gitlab.com").then(|| host.clone()))?;
        Some(Self {
            kind: Kind::GitLab,
            host,
            path: project_path(path)?,
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

    #[cfg(test)]
    pub(crate) fn gitlab(host: &str, path: &str) -> Self {
        Self {
            kind: Kind::GitLab,
            host: host.into(),
            path: path.into(),
        }
    }

    /// Everything before the repository name: a GitHub owner, or a GitLab
    /// namespace with its subgroups.
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

/// The lowercase host, optional port, and raw path of an HTTPS, SSH, or
/// scp-style remote. Credentials in an HTTPS authority are refused outright,
/// so `https://github.com@evil.test/` cannot pass as either host.
fn split(url: &str) -> Option<(String, Option<u16>, &str)> {
    let (authority, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let (authority, path) = rest.split_once('/')?;
        if authority.contains('@') {
            return None;
        }
        (authority, path)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (authority, path) = rest.split_once('/')?;
        (user(authority)?, path)
    } else if !url.contains("://") {
        // scp-like `git@host:group/project.git`; a port cannot be written here.
        let (authority, path) = url.split_once(':')?;
        let authority = user(authority)?;
        if authority.contains(':') {
            return None;
        }
        (authority, path)
    } else {
        return None;
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port.parse::<u16>().ok()?)),
        None => (authority, None),
    };
    let host = host.to_ascii_lowercase();
    let valid = !host.is_empty()
        && host.len() <= MAX_HOST
        && !host.starts_with(['.', '-'])
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    valid.then_some((host, port, path))
}

/// An SSH authority without its user, which may only be a plain name.
fn user(authority: &str) -> Option<&str> {
    match authority.split_once('@') {
        Some((user, host))
            if !user.is_empty()
                && user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) =>
        {
            Some(host)
        }
        Some(_) => None,
        None => Some(authority),
    }
}

/// A GitLab project path: two or more segments, none of which can escape its
/// own place in an API path or web URL.
fn project_path(path: &str) -> Option<String> {
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    let valid = (2..=MAX_SEGMENTS).contains(&segments.len())
        && segments.iter().all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_SEGMENT
                && *segment != "."
                && *segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        // `-` is GitLab's separator between a project and its pages.
        && !segments.contains(&"-");
    valid.then(|| path.to_owned())
}
