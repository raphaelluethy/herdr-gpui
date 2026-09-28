//! GitHub GraphQL through whichever grant is in effect. The native sign-in
//! posts over HTTPS; the CLI runs `gh api graphql` with the same query, so
//! both paths return the same document and share every parser.

use super::{Access, Kind, cli};
use crate::{Error, Result};
use serde_json::Value;
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// One GraphQL variable. Typed, because `gh` sends `-f` values as strings and
/// only `-F` values as numbers, and an `Int!` sent as a string is rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Variable<'a> {
    Text(&'a str),
    Int(u64),
}

pub(crate) fn graphql(
    context: &'static str,
    access: &Access,
    query: &str,
    variables: &[(&str, Variable<'_>)],
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> Result<Value> {
    match access {
        Access::Native(token) => {
            let variables = variables
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        Variable::Text(text) => Value::from(*text),
                        Variable::Int(number) => Value::from(*number),
                    };
                    ((*name).to_owned(), value)
                })
                .collect::<serde_json::Map<_, _>>();
            crate::github::graphql(
                context,
                token,
                query,
                Value::Object(variables),
                timeout,
                cancelled,
                cooldown,
            )
        }
        Access::Gh(program) => gh(
            context, program, query, variables, timeout, &cancelled, cooldown,
        ),
    }
}

/// `gh api graphql` arguments. `-f` sends a value verbatim, so a branch name
/// that starts with `@` is never read as a file; `-F` is used only for numbers.
pub(super) fn gh_arguments(query: &str, variables: &[(&str, Variable<'_>)]) -> Vec<String> {
    let mut arguments = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "--hostname".to_owned(),
        "github.com".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
    ];
    for (name, value) in variables {
        let (flag, value) = match value {
            Variable::Text(text) => ("-f", (*text).to_owned()),
            Variable::Int(number) => ("-F", number.to_string()),
        };
        arguments.push(flag.to_owned());
        arguments.push(format!("{name}={value}"));
    }
    arguments
}

fn gh(
    context: &'static str,
    program: &Path,
    query: &str,
    variables: &[(&str, Variable<'_>)],
    timeout: Duration,
    cancelled: &impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    let output = cli::run(
        Kind::GitHub,
        program,
        &gh_arguments(query, variables),
        deadline,
        cancelled,
    )?;
    if cancelled() {
        return Err(Error::PrCancelled);
    }
    // A GraphQL error exits nonzero but still prints the response document,
    // whose error types say whether the account should pause.
    let document = if output.success || !output.stdout.trim().is_empty() {
        Some(cli::json(Kind::GitHub, context, &output.stdout))
    } else {
        None
    };
    if let Some(Ok(document)) = &document
        && let Some(errors) = document.get("errors")
    {
        let paused = errors.as_array().is_some_and(|errors| {
            errors.iter().any(|error| {
                matches!(
                    error["type"].as_str(),
                    Some("RATE_LIMITED" | "FORBIDDEN" | "UNAUTHORIZED")
                )
            })
        });
        tracing::warn!(
            category = "forge_cli",
            cli = "gh",
            context,
            paused,
            "GitHub CLI query reported errors"
        );
        if paused {
            *cooldown = Some(cli::RATE_LIMIT_COOLDOWN);
        }
        return Err(Error::GitHubQuery);
    }
    if !output.success {
        let (error, pause) = cli::failure(Kind::GitHub, &output);
        tracing::warn!(
            category = "forge_cli",
            cli = "gh",
            context,
            detail = %error,
            "GitHub CLI request failed"
        );
        *cooldown = pause;
        return Err(error);
    }
    document.unwrap_or_else(|| Err(Error::GitHubQuery))
}
