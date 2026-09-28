//! Running the user's own forge CLI. Children get the shared bounded runner's
//! deadline, output cap, and cancellation, but keep the user's CLI auth
//! environment (`GH_TOKEN`, `GH_CONFIG_DIR`, …) because the CLI, not this
//! process, owns the credential. Prompts, pagers, colors, and update notices
//! are turned off so a child can never wait on a terminal it does not have.

use super::Kind;
use crate::{
    Error, Result,
    pull_request::{Output, clean, run_split, strip_git_environment},
};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

/// Rate limits reported by a CLI carry no reset time this client can read, so
/// the account pauses for an hour, as a native reply without headers does.
pub(super) const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(3600);
/// A CLI that stopped being signed in is re-probed well before an hour passes.
pub(super) const SIGNED_OUT_COOLDOWN: Duration = Duration::from_secs(300);
/// Characters of a CLI's own error text kept for a message.
const DIAGNOSTIC_CHARS: usize = 240;

impl Kind {
    fn executable(self) -> &'static str {
        match (self, cfg!(windows)) {
            (Self::GitHub, false) => "gh",
            (Self::GitHub, true) => "gh.exe",
            (Self::GitLab, false) => "glab",
            (Self::GitLab, true) => "glab.exe",
        }
    }
}

/// Find a forge CLI on `PATH`, then where package managers install it: a
/// Finder-launched app gets a minimal `PATH` that often omits Homebrew and Nix.
/// Checks the disk, so call it off the UI thread.
pub(super) fn locate(kind: Kind) -> Option<PathBuf> {
    locate_in(
        kind.executable(),
        std::env::var_os("PATH").as_deref(),
        crate::config::home().ok().as_deref(),
    )
}

pub(super) fn locate_in(name: &str, path: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let fixed = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/home/linuxbrew/.linuxbrew/bin",
        "/nix/var/nix/profiles/default/bin",
        "/run/current-system/sw/bin",
    ];
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(home.into_iter().flat_map(|home| {
            [
                home.join(".local").join("bin"),
                home.join(".nix-profile").join("bin"),
            ]
        }))
        .chain(fixed.into_iter().filter(|_| cfg!(unix)).map(PathBuf::from))
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

/// Run `program` with `args` under the CLI policy. A launch failure because
/// the program is gone reads as "not installed", not as an I/O error.
pub(super) fn run(
    kind: Kind,
    program: &Path,
    args: &[impl AsRef<OsStr>],
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> Result<Output> {
    let mut command = Command::new(program);
    command.args(args);
    policy(&mut command);
    run_split(&mut command, deadline, cancelled, |source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::CliMissing(kind)
        } else {
            Error::PrProcess {
                operation: "launch the forge CLI",
                source,
            }
        }
    })
}

pub(super) fn policy(command: &mut Command) {
    strip_git_environment(command);
    command
        .current_dir("/")
        // Hosts are always named explicitly; a repository or host override
        // from the environment must not redirect a request.
        .env_remove("GH_REPO")
        .env_remove("GH_HOST")
        // Debug output can include request headers.
        .env_remove("GH_DEBUG")
        .env_remove("DEBUG")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("NO_PROMPT", "1")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
}

/// A failed CLI run, as the error this client acts on, plus the cooldown it
/// implies for the account.
pub(super) fn failure(kind: Kind, output: &Output) -> (Error, Option<Duration>) {
    let text = output.stderr.to_ascii_lowercase();
    if text.contains("rate limit") || text.contains("http 429") || text.contains("429 too many") {
        return (Error::CliRateLimit(kind), Some(RATE_LIMIT_COOLDOWN));
    }
    if text.contains("auth login")
        || text.contains("not logged in")
        || text.contains("http 401")
        || text.contains("401 unauthorized")
        || text.contains("bad credentials")
    {
        return (Error::CliSignedOut(kind), Some(SIGNED_OUT_COOLDOWN));
    }
    (
        Error::CliFailed {
            kind,
            details: diagnostic(&output.stderr),
        },
        None,
    )
}

/// A bounded, cleaned, and redacted line from a CLI's stderr, for a message.
pub(super) fn diagnostic(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no details");
    let redacted = redact(line);
    let mut text: String = clean(&redacted).chars().take(DIAGNOSTIC_CHARS).collect();
    if redacted.chars().count() > DIAGNOSTIC_CHARS {
        text.push_str("...");
    }
    text
}

/// Replace anything shaped like a credential. CLIs do not print tokens in
/// errors, but a proxy or a misconfigured host might echo one back.
pub(super) fn redact(text: &str) -> String {
    const PREFIXES: [&str; 11] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "gloas-",
        "gldt-",
        "glrt-",
        "glcbt-",
    ];
    let secret = |word: &str| {
        let bare = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        PREFIXES.iter().any(|prefix| bare.starts_with(prefix))
            || (bare.len() >= 32
                && bare
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')))
    };
    let mut redacted = String::with_capacity(text.len());
    let mut after_scheme = false;
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            redacted.push(' ');
        }
        let assigned = word
            .split_once('=')
            .is_some_and(|(key, _)| key.to_ascii_lowercase().contains("token"));
        if after_scheme || assigned || secret(word) {
            redacted.push_str("[REDACTED]");
        } else {
            redacted.push_str(word);
        }
        after_scheme = word.eq_ignore_ascii_case("bearer");
    }
    redacted
}

/// Parse a CLI's stdout as JSON. The body may hold private repository data, so
/// only its size is logged on failure.
pub(super) fn json(kind: Kind, context: &'static str, stdout: &str) -> Result<serde_json::Value> {
    serde_json::from_str(stdout).map_err(|source| {
        tracing::warn!(
            category = "forge_cli",
            cli = kind.cli(),
            context,
            bytes = stdout.len() as u64,
            "Forge CLI output was not the expected JSON"
        );
        Error::cli_json(kind, source)
    })
}
