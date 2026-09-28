//! Whether the user's forge CLIs are installed and signed in, and as whom.
//! One background thread probes at a time; the UI thread only drains its
//! mailbox. Results are trusted for a while and probed again on expiry or an
//! explicit refresh, so a `gh auth login` in a terminal is picked up.

use super::{Kind, cli, gitlab::Client};
use crate::{Error, Result, avatars::AvatarUpdates, pull_request::clean};
use serde::Deserialize;
use std::{
    path::Path,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

/// How long a probe result stands before the CLI is asked again.
const TTL: Duration = Duration::from_secs(5 * 60);
/// One probe's budget, network round trips included.
const TIMEOUT: Duration = Duration::from_secs(30);
/// GitLab hosts asked about per probe; each costs a request.
const MAX_GITLAB_HOSTS: usize = 4;

/// What a forge CLI can do for this client right now.
#[derive(Clone, Default)]
pub(crate) enum Status {
    /// Not probed yet, or turned off in the config.
    #[default]
    Unknown,
    NotInstalled,
    SignedOut,
    /// Installed, but the probe itself failed; the text says why.
    Failed(String),
    SignedIn(Account),
}

impl Status {
    /// The executable to run, when the CLI can make requests.
    pub(crate) fn program(&self) -> Option<&Arc<Path>> {
        self.account().map(|account| &account.program)
    }

    pub(crate) fn account(&self) -> Option<&Account> {
        match self {
            Self::SignedIn(account) => Some(account),
            _ => None,
        }
    }
}

/// The CLI's signed-in user, as its own API reports it.
#[derive(Clone)]
pub(crate) struct Account {
    pub program: Arc<Path>,
    pub login: String,
    /// The hosts the CLI answered for: `github.com`, or each GitLab instance
    /// `glab` is signed in to.
    pub hosts: Arc<[String]>,
    pub avatar: Option<Arc<gpui::Image>>,
    avatar_url: Option<String>,
}

/// Which CLIs the config allows this client to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Enabled {
    pub gh: bool,
    pub glab: bool,
}

struct Report {
    gh: Status,
    glab: Status,
    avatar_updates: Option<AvatarUpdates>,
}

#[derive(Default)]
pub(crate) struct Probe {
    pub gh: Status,
    pub glab: Status,
    incoming: Option<mpsc::Receiver<Report>>,
    /// When the next probe starts; `None` probes at the next poll.
    due: Option<Instant>,
    enabled: Option<Enabled>,
    avatar_updates: Option<AvatarUpdates>,
}

impl Probe {
    /// Probe again at the next poll, unless a probe is already running.
    pub(crate) fn refresh(&mut self) {
        if self.incoming.is_none() {
            self.due = None;
        }
    }

    /// Drain a finished probe and start the next one when it is due. Never
    /// blocks: finding and running the CLIs happens on the probe's thread.
    pub(crate) fn poll(&mut self, now: Instant, enabled: Enabled) -> bool {
        let mut changed = false;
        if self.enabled != Some(enabled) {
            // A result for the old settings could enable a CLI that is now off.
            self.incoming = None;
            self.due = None;
            for (on, status) in [(enabled.gh, &mut self.gh), (enabled.glab, &mut self.glab)] {
                if !on && !matches!(status, Status::Unknown) {
                    *status = Status::Unknown;
                    changed = true;
                }
            }
            self.enabled = Some(enabled);
        }
        if let Some(incoming) = &self.incoming {
            match incoming.try_recv() {
                Ok(report) => {
                    self.incoming = None;
                    self.due = Some(now + TTL);
                    self.gh = report.gh;
                    self.glab = report.glab;
                    self.avatar_updates = report.avatar_updates;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.incoming = None;
                    self.due = Some(now + TTL);
                    self.failed(enabled);
                    changed = true;
                }
            }
        }
        if let Some(updates) = &self.avatar_updates
            && let Ok(image) = updates.try_recv()
        {
            self.avatar_updates = None;
            if let Status::SignedIn(account) = &mut self.gh {
                account.avatar = Some(image);
                changed = true;
            }
        }
        let wanted = enabled.gh || enabled.glab;
        if self.incoming.is_none() && wanted && self.due.is_none_or(|due| now >= due) {
            self.due = Some(now + TTL);
            let (sender, incoming) = mpsc::sync_channel(1);
            let started = thread::Builder::new()
                .name("herdr-forge-cli".into())
                .spawn(move || {
                    // A dropped receiver means the settings changed; the
                    // report is simply discarded.
                    let _ = sender.send(gather(enabled));
                });
            match started {
                Ok(_) => self.incoming = Some(incoming),
                Err(_) => {
                    self.failed(enabled);
                    changed = true;
                }
            }
        }
        changed
    }

    fn failed(&mut self, enabled: Enabled) {
        let reason = Error::GitHubWorker("CLI probe").to_string();
        for (on, status) in [(enabled.gh, &mut self.gh), (enabled.glab, &mut self.glab)] {
            if on {
                *status = Status::Failed(reason.clone());
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn signed_in_fixture(
        kind: Kind,
        program: &Path,
        login: &str,
        hosts: &[&str],
    ) -> Self {
        let status = Status::SignedIn(Account {
            program: program.into(),
            login: login.into(),
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            avatar: None,
            avatar_url: None,
        });
        let (gh, glab) = match kind {
            Kind::GitHub => (status, Status::Unknown),
            Kind::GitLab => (Status::Unknown, status),
        };
        Self {
            gh,
            glab,
            enabled: Some(Enabled {
                gh: kind == Kind::GitHub,
                glab: kind == Kind::GitLab,
            }),
            due: Some(Instant::now() + TTL),
            ..Self::default()
        }
    }
}

fn gather(enabled: Enabled) -> Report {
    let deadline = Instant::now() + TIMEOUT;
    let probe = |on: bool, kind: Kind, run: fn(&Path, Instant) -> Status| {
        if !on {
            return Status::Unknown;
        }
        match cli::locate(kind) {
            Some(program) => run(&program, deadline),
            None => Status::NotInstalled,
        }
    };
    let mut report = Report {
        gh: probe(enabled.gh, Kind::GitHub, gh),
        glab: probe(enabled.glab, Kind::GitLab, glab),
        avatar_updates: None,
    };
    if let Status::SignedIn(account) = &mut report.gh
        && let Some(url) = account.avatar_url.as_deref()
    {
        // The image transport sends no credentials and accepts only GitHub's
        // avatar host; see `avatars::profile_avatar`. GitLab avatars live on
        // arbitrary hosts, so none is fetched for glab.
        let (avatar, updates) = crate::avatars::profile_avatar(url);
        account.avatar = avatar;
        report.avatar_updates = updates;
    }
    report
}

/// Probe `gh` at `program`: signed in to github.com, and as whom.
pub(super) fn gh(program: &Path, deadline: Instant) -> Status {
    let status = cli::run(
        Kind::GitHub,
        program,
        &["auth", "status", "--hostname", "github.com"],
        deadline,
        &|| false,
    );
    match status {
        Err(Error::CliMissing(_)) => return Status::NotInstalled,
        Err(error) => return Status::Failed(error.to_string()),
        Ok(output) if !output.success => return Status::SignedOut,
        Ok(_) => {}
    }
    match gh_user(program, deadline) {
        Ok(account) => {
            tracing::info!(
                category = "forge_cli",
                cli = "gh",
                login = account.login.as_str(),
                "GitHub CLI account found"
            );
            Status::SignedIn(account)
        }
        Err(Error::CliSignedOut(_)) => Status::SignedOut,
        Err(Error::CliMissing(_)) => Status::NotInstalled,
        Err(error) => Status::Failed(error.to_string()),
    }
}

fn gh_user(program: &Path, deadline: Instant) -> Result<Account> {
    #[derive(Deserialize)]
    struct User {
        login: String,
        #[serde(default)]
        avatar_url: Option<String>,
    }
    let output = cli::run(
        Kind::GitHub,
        program,
        &["api", "user", "--hostname", "github.com"],
        deadline,
        &|| false,
    )?;
    if !output.success {
        return Err(cli::failure(Kind::GitHub, &output).0);
    }
    let user: User = serde_json::from_value(cli::json(Kind::GitHub, "user", &output.stdout)?)
        .map_err(|source| Error::cli_json(Kind::GitHub, source))?;
    Ok(Account {
        program: program.into(),
        login: login(Kind::GitHub, &user.login)?,
        hosts: Arc::from(["github.com".to_owned()]),
        avatar: None,
        avatar_url: user.avatar_url,
    })
}

/// Probe `glab` at `program`: the hosts it is signed in to that also answer
/// for their user, and the first one's username.
pub(super) fn glab(program: &Path, deadline: Instant) -> Status {
    let output = match cli::run(
        Kind::GitLab,
        program,
        &["auth", "status"],
        deadline,
        &|| false,
    ) {
        Err(Error::CliMissing(_)) => return Status::NotInstalled,
        Err(error) => return Status::Failed(error.to_string()),
        Ok(output) => output,
    };
    // glab reports on stderr and exits nonzero when any one host fails, so
    // the per-host lines decide, not the exit status.
    let hosts = signed_in_hosts(&format!("{}\n{}", output.stdout, output.stderr));
    if hosts.is_empty() {
        return Status::SignedOut;
    }
    let mut answered = Vec::new();
    let mut first_login = None;
    let mut failure = None;
    for host in hosts {
        match glab_user(program, &host, deadline) {
            Ok(user) => {
                first_login.get_or_insert(user);
                answered.push(host);
            }
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    match (first_login, failure) {
        (Some(login), _) => {
            tracing::info!(
                category = "forge_cli",
                cli = "glab",
                login = login.as_str(),
                hosts = answered.len() as u64,
                "GitLab CLI account found"
            );
            Status::SignedIn(Account {
                program: program.into(),
                login,
                hosts: answered.into(),
                avatar: None,
                avatar_url: None,
            })
        }
        (None, Some(Error::CliSignedOut(_)) | None) => Status::SignedOut,
        (None, Some(Error::CliMissing(_))) => Status::NotInstalled,
        (None, Some(error)) => Status::Failed(error.to_string()),
    }
}

fn glab_user(program: &Path, host: &str, deadline: Instant) -> Result<String> {
    let client = Client { program, host };
    let user = client.get("gitlab_user", "user", deadline, &|| false, &mut None)?;
    login(Kind::GitLab, user["username"].as_str().unwrap_or_default())
}

fn login(kind: Kind, value: &str) -> Result<String> {
    let login = clean(value).trim().to_owned();
    if login.is_empty() {
        return Err(Error::CliFailed {
            kind,
            details: "no login in the account reply".into(),
        });
    }
    Ok(login)
}

/// The hosts `glab auth status` says it is logged in to, from lines such as
/// `✓ Logged in to gitlab.com as octo (keyring)`. Bounded and deduplicated;
/// anything that is not a plain host name is ignored.
pub(super) fn signed_in_hosts(text: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((_, rest)) = line.split_once("Logged in to ") else {
            continue;
        };
        let Some((host, _)) = rest.split_once(" as ") else {
            continue;
        };
        let host = host.to_ascii_lowercase();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host != "github.com"
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
        if valid && !hosts.contains(&host) {
            hosts.push(host);
        }
        if hosts.len() == MAX_GITLAB_HOSTS {
            break;
        }
    }
    hosts
}
