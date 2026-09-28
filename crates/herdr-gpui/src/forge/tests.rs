#![allow(clippy::unwrap_used)]

use super::{
    Access, Forges, Kind, Remote, Variable,
    cli::{self, diagnostic, failure, locate_in, redact},
    github::gh_arguments,
    github_access, probe,
};
use crate::{Error, config::GitHubCli, pull_request::Output};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

fn token(text: &str) -> Arc<secrecy::SecretString> {
    Arc::new(text.into())
}

fn output(success: bool, stdout: &str, stderr: &str) -> Output {
    Output {
        success,
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

#[test]
fn native_sign_in_wins_unless_the_cli_is_preferred() {
    let native = token("native");
    let gh: Arc<Path> = Path::new("/usr/bin/gh").into();
    let pick = |native: Option<&Arc<secrecy::SecretString>>, gh: Option<&Arc<Path>>, mode| {
        github_access(native, gh, mode)
    };
    for mode in [GitHubCli::Auto, GitHubCli::Off] {
        assert!(matches!(
            pick(Some(&native), Some(&gh), mode),
            Some(Access::Native(_))
        ));
    }
    assert!(matches!(
        pick(Some(&native), Some(&gh), GitHubCli::Prefer),
        Some(Access::Gh(_))
    ));
    // Without a native sign-in, auto and prefer both fall back to gh; off never
    // runs the CLI at all.
    for mode in [GitHubCli::Auto, GitHubCli::Prefer] {
        assert!(matches!(pick(None, Some(&gh), mode), Some(Access::Gh(_))));
    }
    assert!(pick(None, Some(&gh), GitHubCli::Off).is_none());
    // Prefer still uses the native account when gh is not signed in.
    assert!(matches!(
        pick(Some(&native), None, GitHubCli::Prefer),
        Some(Access::Native(_))
    ));
    assert!(pick(None, None, GitHubCli::Auto).is_none());
}

#[test]
fn grants_compare_by_identity_never_by_token_text() {
    let first = token("same-text");
    let second = token("same-text");
    assert_eq!(Access::Native(first.clone()), Access::Native(first.clone()));
    assert_ne!(Access::Native(first.clone()), Access::Native(second));
    let gh = |path: &str| Access::Gh(Path::new(path).into());
    assert_eq!(gh("/bin/gh"), gh("/bin/gh"));
    assert_ne!(gh("/bin/gh"), gh("/opt/gh"));
    assert_ne!(Access::Native(first), gh("/bin/gh"));
}

#[test]
fn a_remote_needs_a_grant_for_its_own_forge() {
    let remote = Remote::parse("git@github.com:owner/repo.git").unwrap();
    assert_eq!(
        (remote.owner(), remote.name(), remote.slug()),
        ("owner", "repo", "owner/repo")
    );
    assert!(matches!(
        Forges::default().access(&remote),
        Err(Error::ForgeAccess(Kind::GitHub))
    ));
    let forges = Forges {
        github: Some(Access::Gh(Path::new("/bin/gh").into())),
    };
    assert!(matches!(forges.access(&remote), Ok(Access::Gh(_))));
    assert!(
        forges
            .remote("https://example.invalid/owner/repo")
            .is_none()
    );
}

#[test]
fn gh_arguments_send_text_raw_and_only_numbers_typed() {
    let arguments = gh_arguments(
        "query { viewer { login } }",
        &[
            ("owner", Variable::Text("@me")),
            ("branch", Variable::Text("feat/a=b")),
            ("limit", Variable::Int(2)),
        ],
    );
    assert_eq!(
        arguments,
        [
            "api",
            "graphql",
            "--hostname",
            "github.com",
            "-f",
            "query=query { viewer { login } }",
            // `-f` is verbatim, so `@me` is never read as a file.
            "-f",
            "owner=@me",
            "-f",
            "branch=feat/a=b",
            "-F",
            "limit=2",
        ]
    );
}

#[test]
fn cli_failures_map_to_typed_errors_and_account_cooldowns() {
    let (error, cooldown) = failure(
        Kind::GitHub,
        &output(
            false,
            "",
            "gh: API rate limit exceeded for user. (HTTP 403)\n",
        ),
    );
    assert!(matches!(error, Error::CliRateLimit(Kind::GitHub)));
    assert_eq!(cooldown, Some(cli::RATE_LIMIT_COOLDOWN));
    for stderr in [
        "gh: Bad credentials (HTTP 401)",
        "You are not logged into any GitHub hosts. To log in, run: gh auth login",
    ] {
        let (error, cooldown) = failure(Kind::GitHub, &output(false, "", stderr));
        assert!(
            matches!(error, Error::CliSignedOut(Kind::GitHub)),
            "{stderr}"
        );
        assert_eq!(cooldown, Some(cli::SIGNED_OUT_COOLDOWN));
    }
    let (error, cooldown) = failure(
        Kind::GitHub,
        &output(
            false,
            "",
            "\n  gh: Could not resolve to a Repository (HTTP 404)\nmore\n",
        ),
    );
    assert!(cooldown.is_none());
    match &error {
        Error::CliFailed { kind, details } => {
            assert_eq!(*kind, Kind::GitHub);
            assert_eq!(details, "gh: Could not resolve to a Repository (HTTP 404)");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        error.to_string(),
        "gh request failed: gh: Could not resolve to a Repository (HTTP 404)"
    );
    assert_eq!(
        Error::CliSignedOut(Kind::GitHub).to_string(),
        "gh is not signed in. Run `gh auth login`, or sign in from the GitHub panel."
    );
}

#[test]
fn diagnostics_are_bounded_cleaned_and_redacted() {
    for secret in [
        "ghp_0123456789abcdef",
        "github_pat_11AAAA",
        "glpat-abcdefghij",
        "abcdefghijklmnopqrstuvwxyz0123456789",
    ] {
        let text = redact(&format!("failed with {secret}."));
        assert!(!text.contains(secret), "{text}");
        assert!(text.contains("[REDACTED]"));
    }
    assert_eq!(
        redact("Authorization: Bearer abc123 was refused"),
        "Authorization: Bearer [REDACTED] was refused"
    );
    assert_eq!(redact("access_token=abc rejected"), "[REDACTED] rejected");
    assert_eq!(redact("HTTP 404: Not Found"), "HTTP 404: Not Found");
    let long = diagnostic(&"x ".repeat(400));
    assert!(long.chars().count() <= 243 && long.ends_with("..."));
    assert_eq!(diagnostic("bad\u{202e}text\n"), "bad text");
    assert_eq!(diagnostic(""), "no details");
}

#[test]
fn locating_a_cli_checks_path_then_package_manager_homes() {
    let root = tempfile::tempdir().unwrap();
    let path_dir = root.path().join("path");
    let home = root.path().join("home");
    let local = home.join(".local").join("bin");
    std::fs::create_dir_all(&path_dir).unwrap();
    std::fs::create_dir_all(&local).unwrap();
    let path = std::env::join_paths([&path_dir]).unwrap();
    std::fs::write(local.join("gh"), "").unwrap();
    assert_eq!(
        locate_in("gh", Some(&path), Some(&home)),
        Some(local.join("gh"))
    );
    std::fs::write(path_dir.join("gh"), "").unwrap();
    assert_eq!(
        locate_in("gh", Some(&path), Some(&home)),
        Some(path_dir.join("gh"))
    );
    // A relative PATH entry could resolve against any working directory.
    let relative = std::env::join_paths(["relative"]).unwrap();
    assert_eq!(
        locate_in("gh", Some(&relative), Some(&home)),
        Some(local.join("gh"))
    );
}

/// A fake CLI: a shell script written by a separate process, so a concurrent
/// test's fork cannot hold it open for writing (Linux ETXTBSY).
#[cfg(unix)]
struct Fake {
    root: tempfile::TempDir,
    program: PathBuf,
}

#[cfg(unix)]
impl Fake {
    fn new(name: &str, body: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join(name);
        let log = root.path().join("log");
        let script = format!(
            "#!/bin/sh\nLOG='{}'\nfor argument in \"$@\"; do printf '%s\\n' \"$argument\" >> \"$LOG\"; done\n{body}\n",
            log.display()
        );
        let status = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$1\" > \"$2\"",
                "write-fake-cli",
                &script,
            ])
            .arg(&program)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, program }
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn access(&self) -> Access {
        Access::Gh(self.program.as_path().into())
    }
}

#[cfg(unix)]
fn query(access: &Access, cooldown: &mut Option<Duration>) -> crate::Result<serde_json::Value> {
    super::graphql(
        "test",
        access,
        "query { viewer { login } }",
        &[("owner", Variable::Text("o")), ("limit", Variable::Int(2))],
        Duration::from_secs(10),
        || false,
        cooldown,
    )
}

#[cfg(unix)]
#[test]
fn gh_graphql_reads_stdout_apart_from_stderr_noise() {
    let fake = Fake::new(
        "gh",
        "echo 'A new release of gh is available' >&2\nprintf '{\"data\":{\"viewer\":{\"login\":\"octo\"}}}'",
    );
    let mut cooldown = None;
    let document = query(&fake.access(), &mut cooldown).unwrap();
    assert_eq!(document["data"]["viewer"]["login"], "octo");
    assert!(cooldown.is_none());
    let log = fake.log();
    assert_eq!(&log[..4], ["api", "graphql", "--hostname", "github.com"]);
    assert!(log.windows(2).any(|pair| pair == ["-F", "limit=2"]));
    assert!(log.windows(2).any(|pair| pair == ["-f", "owner=o"]));
}

#[cfg(unix)]
#[test]
fn gh_graphql_errors_and_failures_pause_the_account_as_native_ones_do() {
    let fake = Fake::new(
        "gh",
        "printf '{\"errors\":[{\"type\":\"RATE_LIMITED\",\"message\":\"slow down\"}]}'\necho 'gh: slow down' >&2\nexit 1",
    );
    let mut cooldown = None;
    assert!(matches!(
        query(&fake.access(), &mut cooldown),
        Err(Error::GitHubQuery)
    ));
    assert_eq!(cooldown, Some(cli::RATE_LIMIT_COOLDOWN));

    let fake = Fake::new("gh", "echo 'gh: Bad credentials (HTTP 401)' >&2\nexit 1");
    let mut cooldown = None;
    assert!(matches!(
        query(&fake.access(), &mut cooldown),
        Err(Error::CliSignedOut(Kind::GitHub))
    ));
    assert_eq!(cooldown, Some(cli::SIGNED_OUT_COOLDOWN));

    let fake = Fake::new("gh", "printf 'not json'");
    assert!(matches!(
        query(&fake.access(), &mut None),
        Err(Error::CliJson {
            kind: Kind::GitHub,
            ..
        })
    ));

    let missing = Access::Gh(Path::new("/nonexistent/herdr-test/gh").into());
    assert!(matches!(
        query(&missing, &mut None),
        Err(Error::CliMissing(Kind::GitHub))
    ));
}

#[cfg(unix)]
#[test]
fn cli_children_keep_their_auth_but_never_prompt_page_or_read_stdin() {
    let fake = Fake::new(
        "gh",
        "printf '%s|%s|%s|%s|%s|' \"$GH_PROMPT_DISABLED\" \"$NO_COLOR\" \"$GH_PAGER\" \"$PAGER\" \"$(pwd)\"\nif read -r line; then printf 'stdin'; else printf 'eof'; fi",
    );
    let output = cli::run(
        Kind::GitHub,
        &fake.program,
        &["version"],
        Instant::now() + Duration::from_secs(10),
        &|| false,
    )
    .unwrap();
    assert!(output.success);
    assert_eq!(output.stdout, "1|1|cat|cat|/|eof");
    // The policy removes only request-redirecting and debug variables; the
    // user's CLI credentials and config directory pass through untouched.
    let mut command = std::process::Command::new("gh");
    cli::policy(&mut command);
    let removed: Vec<_> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    for kept in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_CONFIG_DIR",
        "GH_ENTERPRISE_TOKEN",
    ] {
        assert!(!removed.iter().any(|key| key == kept), "{kept}");
    }
    for dropped in ["GH_HOST", "GH_REPO", "GH_DEBUG"] {
        assert!(removed.iter().any(|key| key == dropped), "{dropped}");
    }
}

#[cfg(unix)]
#[test]
fn cli_runs_are_cancelled_and_bounded_by_their_deadline() {
    let fake = Fake::new("gh", "sleep 5");
    let started = Instant::now();
    assert!(matches!(
        cli::run(
            Kind::GitHub,
            &fake.program,
            &["api"],
            Instant::now() + Duration::from_millis(200),
            &|| false,
        ),
        Err(Error::PrTimeout)
    ));
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(matches!(
        cli::run(
            Kind::GitHub,
            &fake.program,
            &["api"],
            Instant::now() + Duration::from_secs(10),
            &|| true,
        ),
        Err(Error::PrCancelled)
    ));
}

#[cfg(unix)]
#[test]
fn gh_probe_reports_account_signed_out_and_missing() {
    use probe::Status;
    let deadline = || Instant::now() + Duration::from_secs(10);
    let fake = Fake::new(
        "gh",
        "case \"$1\" in\n  auth) exit 0;;\n  api) printf '{\"login\":\"octo\\\\n\",\"avatar_url\":\"https://avatars.githubusercontent.com/u/1\"}';;\nesac",
    );
    match probe::gh(&fake.program, deadline()) {
        Status::SignedIn(account) => {
            assert_eq!(account.login, "octo");
            assert_eq!(&*account.program, fake.program.as_path());
            assert!(account.avatar.is_none(), "images load outside the probe");
        }
        _ => panic!("expected a signed-in account"),
    }
    assert_eq!(
        fake.log(),
        [
            "auth",
            "status",
            "--hostname",
            "github.com",
            "api",
            "user",
            "--hostname",
            "github.com"
        ]
    );
    let fake = Fake::new(
        "gh",
        "echo 'You are not logged into any GitHub hosts.' >&2\nexit 1",
    );
    assert!(matches!(
        probe::gh(&fake.program, deadline()),
        Status::SignedOut
    ));
    assert!(matches!(
        probe::gh(Path::new("/nonexistent/herdr-test/gh"), deadline()),
        Status::NotInstalled
    ));
    let fake = Fake::new(
        "gh",
        "case \"$1\" in\n  auth) exit 0;;\n  api) echo 'gh: HTTP 502' >&2; exit 1;;\nesac",
    );
    assert!(matches!(
        probe::gh(&fake.program, deadline()),
        Status::Failed(reason) if reason.contains("HTTP 502")
    ));
}

#[test]
fn a_disabled_cli_is_forgotten_and_never_probed() {
    let mut probe = super::Probe::signed_in_fixture(Path::new("/bin/gh"), "octo");
    assert!(probe.gh.program().is_some());
    assert!(probe.poll(Instant::now(), super::Enabled { gh: false }));
    assert!(probe.gh.program().is_none());
    // Disabled: nothing is due and nothing starts.
    assert!(!probe.poll(Instant::now(), super::Enabled { gh: false }));
}
