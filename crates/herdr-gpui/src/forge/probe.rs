//! Whether the user's forge CLIs are installed and signed in, and as whom.
//! One background thread probes at a time; the UI thread only drains its
//! mailbox. Results are trusted for a while and probed again on expiry or an
//! explicit refresh, so a `gh auth login` in a terminal is picked up.

use super::{Kind, cli};
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
        match self {
            Self::SignedIn(account) => Some(&account.program),
            _ => None,
        }
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
    pub avatar: Option<Arc<gpui::Image>>,
    avatar_url: Option<String>,
}

/// Which CLIs the config allows this client to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Enabled {
    pub gh: bool,
}

struct Report {
    gh: Status,
    avatar_updates: Option<AvatarUpdates>,
}

#[derive(Default)]
pub(crate) struct Probe {
    pub gh: Status,
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
            if !enabled.gh && !matches!(self.gh, Status::Unknown) {
                self.gh = Status::Unknown;
                changed = true;
            }
            self.enabled = Some(enabled);
        }
        if let Some(incoming) = &self.incoming {
            match incoming.try_recv() {
                Ok(report) => {
                    self.incoming = None;
                    self.due = Some(now + TTL);
                    self.gh = report.gh;
                    self.avatar_updates = report.avatar_updates;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.incoming = None;
                    self.due = Some(now + TTL);
                    self.gh = Status::Failed(Error::GitHubWorker("CLI probe").to_string());
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
        if self.incoming.is_none() && enabled.gh && self.due.is_none_or(|due| now >= due) {
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
                    self.gh = Status::Failed(Error::GitHubWorker("CLI probe").to_string());
                    changed = true;
                }
            }
        }
        changed
    }

    #[cfg(test)]
    pub(crate) fn signed_in_fixture(program: &Path, login: &str) -> Self {
        Self {
            gh: Status::SignedIn(Account {
                program: program.into(),
                login: login.into(),
                avatar: None,
                avatar_url: None,
            }),
            enabled: Some(Enabled { gh: true }),
            due: Some(Instant::now() + TTL),
            ..Self::default()
        }
    }
}

fn gather(enabled: Enabled) -> Report {
    let deadline = Instant::now() + TIMEOUT;
    let gh = if enabled.gh {
        match cli::locate(Kind::GitHub) {
            Some(program) => gh(&program, deadline),
            None => Status::NotInstalled,
        }
    } else {
        Status::Unknown
    };
    let mut report = Report {
        gh,
        avatar_updates: None,
    };
    if let Status::SignedIn(account) = &mut report.gh
        && let Some(url) = account.avatar_url.as_deref()
    {
        // The image transport sends no credentials and accepts only GitHub's
        // avatar host; see `avatars::profile_avatar`.
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
    match user(program, deadline) {
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

fn user(program: &Path, deadline: Instant) -> Result<Account> {
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
    let login = clean(&user.login).trim().to_owned();
    if login.is_empty() {
        return Err(Error::CliFailed {
            kind: Kind::GitHub,
            details: "no login in the account reply".into(),
        });
    }
    Ok(Account {
        program: program.into(),
        login,
        avatar: None,
        avatar_url: user.avatar_url,
    })
}
