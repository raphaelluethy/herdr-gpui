//! Fetching a PR: the GraphQL query, the Git commands that identify the
//! checkout, and the bounded subprocess policy they all run under. Output is
//! size-capped and every call has a deadline, so no step can hang the worker.

use super::{Input, Origin, Result, parse_graphql};
use crate::{
    Error,
    forge::{Access, Forges, Remote, Variable},
};
#[cfg(unix)]
use std::os::{fd::OwnedFd, unix::net::UnixStream};
use std::{
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
pub(super) const TIMEOUT: Duration = Duration::from_secs(15);
const QUERY: &str = r#"query($owner: String!, $repo: String!, $branch: String!, $limit: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequests(first: $limit, headRefName: $branch, orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage }
      nodes {
        number url title state isDraft headRefName baseRefName additions deletions
        changedFiles updatedAt mergeStateStatus reviewDecision headRepositoryOwner { login }
        headRepository { name }
        commits(last: 1) { nodes { commit { statusCheckRollup {
          contexts(first: 100) {
            pageInfo { hasNextPage }
            nodes { __typename ... on CheckRun { status conclusion } ... on StatusContext { state } }
          }
        } } } }
      }
    }
  }
}"#;

/// How long a remote repository's origin is trusted before SSH reads it again.
const ORIGIN_TTL: Duration = Duration::from_secs(10 * 60);
const ORIGIN_LIMIT: usize = 64;

/// Origin remote URLs read on saved hosts, keyed by SSH target and Git
/// directory. Bounded, and owned by the single PR worker thread. The URL, not
/// its parse, is kept: which forge it names can change with the grants held.
#[derive(Default)]
pub(super) struct Origins(Vec<(String, String, Instant, String)>);

impl Origins {
    fn resolve(
        &mut self,
        target: &str,
        input: &Input,
        now: Instant,
        deadline: Instant,
        cancelled: &impl Fn() -> bool,
    ) -> crate::Result<String> {
        self.0
            .retain(|(_, _, resolved, _)| now.duration_since(*resolved) < ORIGIN_TTL);
        if let Some((.., url)) = self
            .0
            .iter()
            .find(|(host, key, ..)| host == target && key == &input.repo_key)
        {
            return Ok(url.clone());
        }
        let timeout = deadline
            .checked_duration_since(now)
            .ok_or(Error::PrTimeout)?;
        // The daemon's branch is trusted as reported: this client cannot run
        // local Git against the host's checkout to re-verify it.
        let url = herdr_client::remote_origin_url(target, &input.repo_key, timeout, cancelled)?
            .ok_or(Error::PrOrigin)?;
        if self.0.len() == ORIGIN_LIMIT {
            self.0.remove(0);
        }
        self.0
            .push((target.to_owned(), input.repo_key.clone(), now, url.clone()));
        Ok(url)
    }
}

#[cfg(test)]
pub(super) fn fetch(input: &Input, forges: &Forges, cancelled: impl Fn() -> bool) -> Result {
    fetch_with_backoff(
        input,
        &Origin::Local,
        &mut Origins::default(),
        forges,
        cancelled,
        &mut None,
    )
}

pub(super) fn fetch_with_backoff(
    input: &Input,
    origin: &Origin,
    origins: &mut Origins,
    forges: &Forges,
    cancelled: impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> Result {
    let deadline = Instant::now() + TIMEOUT;
    let (remote, head) = match origin {
        Origin::Local => {
            let checkout = local_checkout(input, deadline, &cancelled)?;
            let remote = origin_remote(&checkout, forges, deadline, &cancelled)?;
            let head = upstream_head(&input.branch, forges, |key| {
                let value = git(
                    &checkout,
                    &["config", "--default", "", "--get", key],
                    deadline,
                    &cancelled,
                )?;
                Ok((!value.is_empty()).then_some(value))
            })?;
            (remote, head)
        }
        Origin::Ssh(target) => {
            let url = origins.resolve(target, input, Instant::now(), deadline, &cancelled)?;
            let remote = parse_origin(&url, forges)?;
            let head = upstream_head(&input.branch, forges, |key| {
                let timeout = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or(Error::PrTimeout)?;
                Ok(herdr_client::remote_config_value(
                    target,
                    &input.repo_key,
                    key,
                    timeout,
                    &cancelled,
                )?)
            })?;
            (remote, head)
        }
    };
    let access = forges.access(&remote)?;
    let branch = head
        .as_ref()
        .map_or(input.branch.as_str(), |head| head.branch.as_str());
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .ok_or(Error::PrTimeout)?;
    github(
        &remote,
        access,
        branch,
        head.as_ref(),
        timeout,
        cancelled,
        cooldown,
    )
}

fn github(
    remote: &Remote,
    access: &Access,
    branch: &str,
    head: Option<&Head>,
    timeout: Duration,
    cancelled: impl Fn() -> bool,
    cooldown: &mut Option<Duration>,
) -> Result {
    let (owner, repo) = (remote.owner(), remote.name());
    let response = crate::forge::graphql(
        "pull_request",
        access,
        QUERY,
        &[
            ("owner", Variable::Text(owner)),
            ("repo", Variable::Text(repo)),
            ("branch", Variable::Text(branch)),
            ("limit", Variable::Int(if head.is_some() { 100 } else { 2 })),
        ],
        timeout,
        cancelled,
        cooldown,
    )?;
    parse_graphql(response, owner, repo, branch, head)
}

/// The remote head configured for this local branch, independent of its local name.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Head {
    pub remote: Remote,
    pub branch: String,
}

pub(super) fn upstream_head(
    branch: &str,
    forges: &Forges,
    mut config: impl FnMut(&str) -> crate::Result<Option<String>>,
) -> crate::Result<Option<Head>> {
    let remote = config(&format!("branch.{branch}.remote"))?;
    let merge = config(&format!("branch.{branch}.merge"))?;
    let (Some(remote), Some(merge)) = (remote, merge) else {
        return Ok(None);
    };
    // An explicitly configured but unsupported upstream must not fall back to
    // a potentially unrelated origin branch (including local `.` upstreams).
    let branch = merge
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
        .ok_or(Error::PrBranch)?;
    let url = config(&format!("remote.{remote}.url"))?.ok_or(Error::PrOrigin)?;
    Ok(Some(Head {
        remote: forges.remote(&url).ok_or(Error::PrOrigin)?,
        branch: branch.to_owned(),
    }))
}

pub(crate) fn local_repository(
    input: &Input,
    forges: &Forges,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<Remote> {
    let checkout = local_checkout(input, deadline, cancelled)?;
    origin_remote(&checkout, forges, deadline, cancelled)
}

/// The forge repository behind a verified checkout's origin remote.
pub(crate) fn origin_remote(
    checkout: &str,
    forges: &Forges,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<Remote> {
    let url = git(
        checkout,
        &["config", "--get", "remote.origin.url"],
        deadline,
        cancelled,
    )?;
    parse_origin(&url, forges)
}

fn parse_origin(url: &str, forges: &Forges) -> crate::Result<Remote> {
    forges.remote(url).ok_or_else(|| {
        // An SSH host alias for a second account (`github-work:owner/repo`)
        // is the usual reason; only the host is logged, never credentials.
        tracing::debug!(
            category = "github_origin",
            host = remote_host(url),
            "Origin remote is not a supported forge repository"
        );
        Error::PrOrigin
    })
}

/// The host of a Git remote, without the user, credentials, or path.
pub(super) fn remote_host(remote: &str) -> &str {
    let authority = match remote.split_once("://") {
        Some((_, rest)) => rest.split('/').next().unwrap_or_default(),
        None => remote.split(':').next().unwrap_or_default(),
    };
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// Resolve the checkout a daemon workspace names and verify it still is that
/// repository on that branch. Every local Git operation starts here, so a
/// renamed branch or a moved worktree cannot be worked on by mistake.
pub(crate) fn local_checkout(
    input: &Input,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<String> {
    if input
        .checkout
        .as_ref()
        .is_some_and(|path| !Path::new(path).is_absolute())
        || !Path::new(&input.repo_key).is_absolute()
    {
        return Err(Error::PrAbsolutePath);
    }
    if input.branch.is_empty()
        || input.branch.len() > 1024
        || input.branch.chars().any(char::is_control)
    {
        return Err(Error::PrBranch);
    }
    let checkout = match &input.checkout {
        Some(path) => path.clone(),
        None => {
            // Older daemons lack workspace.get. Use Git's own worktree registry,
            // never pane cwd or the daemon's new-workspace directory policy.
            let mut command = Command::new("git");
            command.args([
                "-c",
                "core.fsmonitor=false",
                "--git-dir",
                &input.repo_key,
                "worktree",
                "list",
                "--porcelain",
                "-z",
            ]);
            let (ok, output) = run(&mut command, deadline, cancelled)?;
            if !ok {
                return Err(Error::PrWorktreeLookup);
            }
            worktree_checkout(&output, &input.branch)?
        }
    };
    // A Git registry candidate still must match both repository and live HEAD.
    let common = git(
        &checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        deadline,
        cancelled,
    )?;
    if Path::new(&common)
        .canonicalize()
        .ok()
        .zip(Path::new(&input.repo_key).canonicalize().ok())
        .is_none_or(|(actual, expected)| actual != expected)
    {
        return Err(Error::PrRepositoryMismatch);
    }
    if git(
        &checkout,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        deadline,
        cancelled,
    )? != input.branch
    {
        return Err(Error::PrBranchChanged);
    }
    Ok(checkout)
}

/// Read-only Git output from a checkout, with the shared process policy.
fn git(
    checkout: &str,
    args: &[&str],
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.fsmonitor=false", "-C", checkout])
        .args(args);
    run(&mut command, deadline, cancelled).and_then(|(ok, output)| {
        if ok {
            Ok(output.trim_end_matches(['\r', '\n']).to_owned())
        } else {
            Err(Error::PrCheckout)
        }
    })
}

pub(super) fn worktree_checkout(output: &str, branch: &str) -> crate::Result<String> {
    let branch = format!("branch refs/heads/{branch}");
    let mut paths = output.split("\0\0").filter_map(|record| {
        let mut fields = record.split('\0');
        let path = fields.next()?.strip_prefix("worktree ")?;
        (Path::new(path).is_absolute() && fields.any(|field| field == branch)).then_some(path)
    });
    let path = paths.next().ok_or(Error::PrMissingWorktree)?;
    if paths.next().is_some() {
        return Err(Error::PrAmbiguousWorktree);
    }
    Ok(path.into())
}

/// Run a Git child under the shared process policy: no inherited `GIT_*`
/// state, no prompts, no stdin, a neutral working directory, and stdout and
/// stderr merged into one bounded stream.
pub(crate) fn run(
    command: &mut Command,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<(bool, String)> {
    if cancelled() {
        return Err(Error::PrCancelled);
    }
    strip_git_environment(command);
    command
        .current_dir("/")
        .env_remove("GH_REPO")
        .env_remove("GH_DEBUG")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env("GH_HOST", "github.com")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null());
    let (success, mut streams) = bounded(command, Streams::Merged, deadline, cancelled, spawned)?;
    let output = streams.pop().unwrap_or_default();
    String::from_utf8(output)
        .map(|text| (success, text))
        .map_err(|error| Error::PrEncoding(error.utf8_error()))
}

/// A child's exit and its two output streams, read apart so a machine-readable
/// stdout is never interleaved with diagnostics.
#[derive(Debug, Default)]
pub(crate) struct Output {
    pub success: bool,
    pub stdout: String,
    /// At most [`DIAGNOSTIC_LIMIT`] bytes; the rest is drained and dropped.
    pub stderr: String,
}

/// Stderr kept for a diagnostic. It is read to the end either way, so a chatty
/// child cannot block on a full pipe, but only this much is retained.
pub(crate) const DIAGNOSTIC_LIMIT: usize = 64 * 1024;

/// Run a child whose caller has already set its environment policy, with the
/// same deadline, cancellation, and output cap as [`run`], but with stdout and
/// stderr kept apart. `spawned` names a launch failure in the caller's terms.
pub(crate) fn run_split(
    command: &mut Command,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    spawned: impl FnOnce(std::io::Error) -> Error,
) -> crate::Result<Output> {
    if cancelled() {
        return Err(Error::PrCancelled);
    }
    command.stdin(Stdio::null());
    let (success, mut streams) = bounded(command, Streams::Split, deadline, cancelled, spawned)?;
    let stderr = streams.pop().unwrap_or_default();
    let stdout = streams.pop().unwrap_or_default();
    Ok(Output {
        success,
        stdout: String::from_utf8(stdout).map_err(|error| Error::PrEncoding(error.utf8_error()))?,
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// `GIT_*` variables can point a child at another repository or change what
/// it prints, so no child inherits them.
pub(crate) fn strip_git_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
}

#[derive(Clone, Copy)]
enum Streams {
    /// Stdout and stderr share one stream, capped at [`OUTPUT_LIMIT`].
    Merged,
    /// Stdout capped at [`OUTPUT_LIMIT`], stderr truncated at [`DIAGNOSTIC_LIMIT`].
    Split,
}

impl Streams {
    fn sinks<R>(self, readers: Vec<R>) -> Vec<Sink<R>> {
        readers
            .into_iter()
            .enumerate()
            .map(|(index, reader)| Sink {
                reader,
                bytes: Vec::new(),
                eof: false,
                limit: match (self, index) {
                    (Self::Split, 1) => Limit::Truncate(DIAGNOSTIC_LIMIT),
                    _ => Limit::Fail(OUTPUT_LIMIT),
                },
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
enum Limit {
    /// Exceeding the bound fails the whole run.
    Fail(usize),
    /// Bytes past the bound are read and dropped.
    Truncate(usize),
}

impl Limit {
    fn push(self, bytes: &mut Vec<u8>, chunk: &[u8]) -> crate::Result<()> {
        match self {
            Self::Fail(limit) if bytes.len() + chunk.len() > limit => Err(Error::PrSize),
            Self::Fail(_) => {
                bytes.extend_from_slice(chunk);
                Ok(())
            }
            Self::Truncate(limit) => {
                let room = limit.saturating_sub(bytes.len()).min(chunk.len());
                bytes.extend_from_slice(&chunk[..room]);
                Ok(())
            }
        }
    }
}

struct Sink<R> {
    reader: R,
    bytes: Vec<u8>,
    eof: bool,
    limit: Limit,
}

/// Spawn, read under the deadline, and always reap the exact child created
/// here, killing it first when reading failed or was cancelled.
fn bounded(
    command: &mut Command,
    streams: Streams,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
    spawned: impl FnOnce(std::io::Error) -> Error,
) -> crate::Result<(bool, Vec<Vec<u8>>)> {
    let (readers, mut child) = capture(command, streams, spawned)?;
    // Command retains Stdio descriptors after spawn; release them so EOF is observable.
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let result = collect(streams.sinks(readers), &mut child, deadline, cancelled);
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn spawned(source: std::io::Error) -> Error {
    Error::PrProcess {
        operation: "launch Git (install git on PATH)",
        source,
    }
}

fn unreadable(source: std::io::Error) -> Error {
    Error::PrProcess {
        operation: "read process output",
        source,
    }
}

fn channel_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::PrProcess { operation, source }
}

/// Merged output joins stdout and stderr in one stream this process can poll;
/// split output gives each its own.
#[cfg(unix)]
fn capture(
    command: &mut Command,
    streams: Streams,
    spawned: impl FnOnce(std::io::Error) -> Error,
) -> crate::Result<(Vec<UnixStream>, Child)> {
    let pair = || -> crate::Result<(UnixStream, UnixStream)> {
        let (reader, writer) =
            UnixStream::pair().map_err(channel_error("create process output channel"))?;
        reader
            .set_nonblocking(true)
            .map_err(channel_error("configure process output"))?;
        Ok((reader, writer))
    };
    let (reader, writer) = pair()?;
    let mut readers = vec![reader];
    let error_writer = match streams {
        Streams::Merged => writer
            .try_clone()
            .map_err(channel_error("configure process errors"))?,
        Streams::Split => {
            let (error_reader, error_writer) = pair()?;
            readers.push(error_reader);
            error_writer
        }
    };
    command
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::from(OwnedFd::from(error_writer)));
    let child = command.spawn().map_err(spawned)?;
    Ok((readers, child))
}

/// Windows cannot hand a socket to a child as its standard streams, so output
/// travels through anonymous pipes instead.
#[cfg(windows)]
fn capture(
    command: &mut Command,
    streams: Streams,
    spawned: impl FnOnce(std::io::Error) -> Error,
) -> crate::Result<(Vec<std::io::PipeReader>, Child)> {
    let (reader, writer) =
        std::io::pipe().map_err(channel_error("create process output channel"))?;
    let mut readers = vec![reader];
    let error_writer = match streams {
        Streams::Merged => writer
            .try_clone()
            .map_err(channel_error("configure process errors"))?,
        Streams::Split => {
            let (error_reader, error_writer) =
                std::io::pipe().map_err(channel_error("create process error channel"))?;
            readers.push(error_reader);
            error_writer
        }
    };
    command
        .stdout(Stdio::from(writer))
        .stderr(Stdio::from(error_writer));
    let child = command.spawn().map_err(spawned)?;
    Ok((readers, child))
}

/// Reads every stream under the caller's deadline and cancellation. The child
/// is only reaped once all of its output has ended, so nothing is truncated.
#[cfg(unix)]
fn collect(
    mut sinks: Vec<Sink<UnixStream>>,
    child: &mut Child,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<(bool, Vec<Vec<u8>>)> {
    let mut buffer = [0; 8192];
    loop {
        if cancelled() {
            return Err(Error::PrCancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::PrTimeout);
        }
        let mut progressed = false;
        for sink in sinks.iter_mut().filter(|sink| !sink.eof) {
            match sink.reader.read(&mut buffer) {
                Ok(0) => sink.eof = true,
                Ok(n) => {
                    sink.limit.push(&mut sink.bytes, &buffer[..n])?;
                    progressed = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    progressed = true;
                }
                Err(source) => return Err(unreadable(source)),
            }
        }
        if progressed {
            continue;
        }
        if let Some(status) = child
            .try_wait()
            .map_err(channel_error("wait for process"))?
            && sinks.iter().all(|sink| sink.eof)
        {
            return Ok((
                status.success(),
                sinks.into_iter().map(|sink| sink.bytes).collect(),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// An anonymous pipe on Windows cannot be made nonblocking, so each stream is
/// read on its own thread and the deadline is enforced here. Killing the child
/// closes the last writers, which ends those threads.
#[cfg(windows)]
fn collect(
    sinks: Vec<Sink<std::io::PipeReader>>,
    child: &mut Child,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<(bool, Vec<Vec<u8>>)> {
    let (sender, reads) = std::sync::mpsc::channel();
    let count = sinks.len();
    for (index, mut sink) in sinks.into_iter().enumerate() {
        let sender = sender.clone();
        thread::Builder::new()
            .name("herdr-pr-output".into())
            .spawn(move || {
                let mut buffer = [0; 8192];
                let result = loop {
                    match sink.reader.read(&mut buffer) {
                        Ok(0) => break Ok(sink.bytes),
                        Ok(n) => {
                            if let Err(error) = sink.limit.push(&mut sink.bytes, &buffer[..n]) {
                                break Err(error);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(source) => break Err(unreadable(source)),
                    }
                };
                let _ = sender.send((index, result));
            })
            .map_err(unreadable)?;
    }
    drop(sender);
    let mut ended: Vec<Option<Vec<u8>>> = vec![None; count];
    loop {
        if cancelled() {
            return Err(Error::PrCancelled);
        }
        if Instant::now() >= deadline {
            return Err(Error::PrTimeout);
        }
        loop {
            match reads.try_recv() {
                Ok((index, result)) => {
                    if let Some(slot) = ended.get_mut(index) {
                        *slot = Some(result?);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if ended.iter().any(Option::is_none) {
                        return Err(unreadable(std::io::Error::other("output reader stopped")));
                    }
                    break;
                }
            }
        }
        if let Some(status) = child
            .try_wait()
            .map_err(channel_error("wait for process"))?
            && ended.iter().all(Option::is_some)
        {
            return Ok((status.success(), ended.into_iter().flatten().collect()));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod origin_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn input(key: &str) -> Input {
        Input {
            checkout: None,
            repo_key: key.into(),
            branch: "main".into(),
        }
    }

    #[test]
    fn remote_origins_are_reused_until_they_expire() {
        // Time only moves forward here: an `Instant` cannot go before boot.
        let now = Instant::now();
        let deadline = now + TIMEOUT;
        // An invalid target fails before SSH, so reaching it proves a miss.
        let target = "-not-dialled";
        let mut origins = Origins(vec![(
            target.into(),
            "/repo/.git".into(),
            now,
            "git@github.com:owner/repo.git".into(),
        )]);
        let mut resolve = |key: &str, at: Instant| {
            origins.resolve(target, &input(key), at, deadline.max(at + TIMEOUT), &|| {
                false
            })
        };
        assert_eq!(
            resolve("/repo/.git", now).unwrap(),
            "git@github.com:owner/repo.git"
        );
        // Another repository on the same host is not a hit.
        assert!(matches!(
            resolve("/other/.git", now),
            Err(Error::Client(herdr_client::Error::InvalidSshTarget))
        ));
        assert!(resolve("/repo/.git", now + ORIGIN_TTL).is_err());
        assert!(origins.0.is_empty());
    }
}
