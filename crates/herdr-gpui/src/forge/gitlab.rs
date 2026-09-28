//! GitLab's REST API through the user's own `glab api`. Every request names
//! its host explicitly and addresses projects by encoded path or numeric ID,
//! never by the working directory, so a child can only reach the project a
//! verified checkout names.

use super::{Kind, Remote, cli};
use crate::{Error, Result, pull_request::clean};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// RFC 3986 unreserved characters stay as they are; everything else,
/// including `/` in a project path, is escaped.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// One component of an API path or query, escaped.
pub(crate) fn encode(component: &str) -> String {
    utf8_percent_encode(component, COMPONENT).to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Method {
    Get,
    Post,
}

/// `glab api` arguments. Fields use `-f`, which sends them verbatim.
pub(super) fn arguments(
    host: &str,
    method: Method,
    endpoint: &str,
    fields: &[(&str, &str)],
) -> Vec<String> {
    let mut arguments = vec!["api".to_owned(), "--hostname".to_owned(), host.to_owned()];
    if method == Method::Post {
        arguments.extend(["--method".to_owned(), "POST".to_owned()]);
    }
    arguments.push(endpoint.to_owned());
    for (name, value) in fields {
        arguments.push("-f".to_owned());
        arguments.push(format!("{name}={value}"));
    }
    arguments
}

/// The user's `glab`, pointed at one GitLab host.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Client<'a> {
    pub program: &'a Path,
    pub host: &'a str,
}

impl Client<'_> {
    pub(crate) fn get(
        self,
        context: &'static str,
        endpoint: &str,
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
        cooldown: &mut Option<Duration>,
    ) -> Result<Value> {
        self.call(
            context,
            &arguments(self.host, Method::Get, endpoint, &[]),
            deadline,
            cancelled,
            cooldown,
        )
    }

    pub(crate) fn post(
        self,
        context: &'static str,
        endpoint: &str,
        fields: &[(&str, &str)],
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
        cooldown: &mut Option<Duration>,
    ) -> Result<Value> {
        self.call(
            context,
            &arguments(self.host, Method::Post, endpoint, fields),
            deadline,
            cancelled,
            cooldown,
        )
    }

    /// One REST call. A failure's cooldown, if any, is left in `cooldown`.
    fn call(
        self,
        context: &'static str,
        arguments: &[String],
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
        cooldown: &mut Option<Duration>,
    ) -> Result<Value> {
        let output = cli::run(Kind::GitLab, self.program, arguments, deadline, cancelled)?;
        if cancelled() {
            return Err(Error::PrCancelled);
        }
        if !output.success {
            let (error, pause) = cli::failure(Kind::GitLab, &output);
            tracing::warn!(
                category = "forge_cli",
                cli = "glab",
                context,
                detail = %error,
                "GitLab CLI request failed"
            );
            *cooldown = pause;
            return Err(error);
        }
        cli::json(Kind::GitLab, context, &output.stdout)
    }

    /// The project a remote names, as GitLab reports it.
    pub(crate) fn project(
        self,
        remote: &Remote,
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
        cooldown: &mut Option<Duration>,
    ) -> Result<Project> {
        let value = self.get(
            "gitlab_project",
            &format!("projects/{}", encode(&remote.path)),
            deadline,
            cancelled,
            cooldown,
        )?;
        parse_project(&value, remote)
    }
}

/// The project a remote names, as GitLab reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Project {
    pub id: u64,
    /// The project's web address, used as the base of every link built for it.
    pub web_url: String,
    pub default_branch: Option<String>,
}

/// A project reply, rejected unless it is the project that was asked for and
/// its web address stays on the same host.
pub(crate) fn parse_project(value: &Value, remote: &Remote) -> Result<Project> {
    let id = value["id"].as_u64().filter(|id| *id > 0);
    let path = value["path_with_namespace"].as_str();
    let web_url = value["web_url"]
        .as_str()
        .and_then(|url| web_url(url, remote));
    match (id, path, web_url) {
        (Some(id), Some(path), Some(web_url)) if path.eq_ignore_ascii_case(&remote.path) => {
            Ok(Project {
                id,
                web_url,
                default_branch: value["default_branch"]
                    .as_str()
                    .map(clean)
                    .filter(|branch| !branch.is_empty()),
            })
        }
        _ => Err(Error::GitLabProject),
    }
}

/// A project's web address, accepted only on the remote's own host and path.
/// An instance under a relative URL root (`https://host/gitlab/group/repo`)
/// keeps its prefix; a trailing slash is dropped.
fn web_url(url: &str, remote: &Remote) -> Option<String> {
    let url = url.trim_end_matches('/');
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (authority, path) = rest.split_once('/')?;
    let host = |value: &str| {
        value
            .rsplit_once(':')
            .map_or(value, |(host, _)| host)
            .to_ascii_lowercase()
    };
    let expected = format!("/{}", remote.path.to_ascii_lowercase());
    let safe = url.len() <= 2048
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '\\' | '@'));
    (safe
        && host(authority) == host(&remote.host)
        && format!("/{path}").to_ascii_lowercase().ends_with(&expected))
    .then(|| url.to_owned())
}
