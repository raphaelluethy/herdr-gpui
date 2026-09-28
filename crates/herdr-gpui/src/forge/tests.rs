#![allow(clippy::unwrap_used)]

#[cfg(unix)]
use super::fake::Fake;
use super::{
    Access, Forges, Kind, Remote, Variable,
    cli::{self, diagnostic, failure, locate_in, redact},
    github::gh_arguments,
    github_access, probe,
};
use crate::{Error, config::GitHubCli, pull_request::Output};
use std::{
    path::Path,
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
    let remote = Remote::parse("git@github.com:owner/repo.git", &[]).unwrap();
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
        ..Forges::default()
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

#[cfg(unix)]
fn gh(fake: &Fake) -> Access {
    Access::Gh(fake.program.as_path().into())
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
    let document = query(&gh(&fake), &mut cooldown).unwrap();
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
        query(&gh(&fake), &mut cooldown),
        Err(Error::GitHubQuery)
    ));
    assert_eq!(cooldown, Some(cli::RATE_LIMIT_COOLDOWN));

    let fake = Fake::new("gh", "echo 'gh: Bad credentials (HTTP 401)' >&2\nexit 1");
    let mut cooldown = None;
    assert!(matches!(
        query(&gh(&fake), &mut cooldown),
        Err(Error::CliSignedOut(Kind::GitHub))
    ));
    assert_eq!(cooldown, Some(cli::SIGNED_OUT_COOLDOWN));

    let fake = Fake::new("gh", "printf 'not json'");
    assert!(matches!(
        query(&gh(&fake), &mut None),
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
    let mut probe = super::Probe::signed_in_fixture(
        Kind::GitHub,
        Path::new("/bin/gh"),
        "octo",
        &["github.com"],
    );
    assert!(probe.gh.program().is_some());
    let off = super::Enabled {
        gh: false,
        glab: false,
    };
    assert!(probe.poll(Instant::now(), off));
    assert!(probe.gh.program().is_none());
    // Disabled: nothing is due and nothing starts.
    assert!(!probe.poll(Instant::now(), off));
}

fn hosts(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn gitlab_remotes_parse_nested_groups_over_https_ssh_and_scp() {
    let none: Vec<String> = Vec::new();
    for url in [
        "git@gitlab.com:group/subgroup/deeper/project.git",
        "https://gitlab.com/group/subgroup/deeper/project.git",
        "https://gitlab.com/group/subgroup/deeper/project",
        "ssh://git@gitlab.com/group/subgroup/deeper/project.git",
        "ssh://git@GitLab.com:22/group/subgroup/deeper/project.git",
    ] {
        let remote = Remote::parse(url, &none).unwrap_or_else(|| panic!("{url}"));
        assert_eq!(remote.kind, Kind::GitLab, "{url}");
        assert_eq!(remote.host, "gitlab.com", "{url}");
        assert_eq!(remote.path, "group/subgroup/deeper/project", "{url}");
        assert_eq!(remote.owner(), "group/subgroup/deeper");
        assert_eq!(remote.name(), "project");
    }
    // GitHub keeps its strict shape and lowercases the owner, as before.
    let github = Remote::parse("https://github.com/Owner/Repo.git", &none).unwrap();
    assert_eq!(
        (github.kind, github.host.as_str(), github.path.as_str()),
        (Kind::GitHub, "github.com", "owner/Repo")
    );
}

#[test]
fn self_hosted_gitlab_is_recognized_only_where_glab_is_signed_in() {
    let signed_in = hosts(&["gitlab.example.com", "code.example.org:8443"]);
    let remote = Remote::parse("git@gitlab.example.com:team/app.git", &signed_in).unwrap();
    assert_eq!(
        (remote.kind, remote.host.as_str(), remote.path.as_str()),
        (Kind::GitLab, "gitlab.example.com", "team/app")
    );
    // A host glab names with its port is matched through the HTTPS remote's
    // port, and the API is addressed exactly as glab names it.
    let remote = Remote::parse("https://code.example.org:8443/a/b/c.git", &signed_in).unwrap();
    assert_eq!(remote.host, "code.example.org:8443");
    // An SSH port says nothing about which instance serves the API.
    let remote =
        Remote::parse("ssh://git@gitlab.example.com:2222/team/app.git", &signed_in).unwrap();
    assert_eq!(remote.host, "gitlab.example.com");
    assert!(Remote::parse("git@gitlab.other.test:team/app.git", &signed_in).is_none());
    // github.com is never GitLab, even if glab were to list it.
    assert!(Remote::parse("https://github.com/a/b/c", &hosts(&["github.com"])).is_none());
}

#[test]
fn gitlab_remotes_refuse_credentials_traversal_and_malformed_paths() {
    let signed_in = hosts(&["gitlab.example.com"]);
    for url in [
        "https://user:secret@gitlab.com/group/project.git",
        "https://gitlab.com@evil.test/group/project.git",
        "https://evil.test/gitlab.com/group/project",
        "git@gitlab.com.evil.test:group/project.git",
        "gitlab.com:project",
        "https://gitlab.com/project",
        "https://gitlab.com/group/project/",
        "https://gitlab.com/group//project",
        "https://gitlab.com/group/../project",
        "https://gitlab.com/group/project/-/tree/main",
        "https://gitlab.com/group/project?ref=main",
        "https://gitlab.com/group/project#readme",
        "https://gitlab.com/group/pro%2fject",
        "https://gitlab.com:notaport/group/project",
        "file:///gitlab.com/group/project",
        "git@:group/project",
        "bad user@gitlab.com:group/project",
        "https://gitlab.com/group/project\n",
    ] {
        assert!(Remote::parse(url, &signed_in).is_none(), "{url:?}");
    }
    let deep = format!("https://gitlab.com/{}project", "g/".repeat(21));
    assert!(Remote::parse(&deep, &signed_in).is_none());
    let deepest = format!("https://gitlab.com/{}project", "g/".repeat(20));
    assert!(Remote::parse(&deepest, &signed_in).is_some());
}

#[test]
fn gitlab_access_needs_glab_signed_in_to_that_host() {
    let glab = Access::Glab(Path::new("/bin/glab").into());
    let forges = Forges {
        gitlab: Some(glab.clone()),
        gitlab_hosts: hosts(&["gitlab.example.com"]).into(),
        ..Forges::default()
    };
    let hosted = forges
        .remote("git@gitlab.example.com:team/app.git")
        .unwrap();
    assert_eq!(forges.access(&hosted).unwrap(), &glab);
    // gitlab.com is still recognized, but glab is not signed in there.
    let public = forges.remote("git@gitlab.com:team/app.git").unwrap();
    assert!(matches!(
        forges.access(&public),
        Err(Error::ForgeAccess(Kind::GitLab))
    ));
    // Neither grant answers for the other forge.
    let github = forges.remote("git@github.com:owner/repo.git").unwrap();
    assert!(matches!(
        forges.access(&github),
        Err(Error::ForgeAccess(Kind::GitHub))
    ));
    assert_eq!(
        Error::ForgeAccess(Kind::GitLab).to_string(),
        "GitLab access unavailable for this repository. Run `glab auth login` for this host."
    );
}

#[test]
fn glab_arguments_name_the_host_and_encode_paths_and_queries() {
    use super::gitlab::{Method, arguments, encode};
    assert_eq!(
        encode("group/sub group/proj.x"),
        "group%2Fsub%20group%2Fproj.x"
    );
    assert_eq!(encode("feat/a&b=c#d"), "feat%2Fa%26b%3Dc%23d");
    assert_eq!(
        arguments(
            "gitlab.example.com",
            Method::Get,
            "projects/a%2Fb/merge_requests?state=opened",
            &[]
        ),
        [
            "api",
            "--hostname",
            "gitlab.example.com",
            "projects/a%2Fb/merge_requests?state=opened"
        ]
    );
    assert_eq!(
        arguments(
            "gitlab.com",
            Method::Post,
            "projects/7/merge_requests",
            &[("title", "@not-a-file"), ("source_branch", "feature")]
        ),
        [
            "api",
            "--hostname",
            "gitlab.com",
            "--method",
            "POST",
            "projects/7/merge_requests",
            "-f",
            "title=@not-a-file",
            "-f",
            "source_branch=feature"
        ]
    );
}

#[test]
fn gitlab_projects_must_be_the_one_asked_for_on_the_same_host() {
    use super::gitlab::parse_project;
    let remote = Remote::gitlab("gitlab.example.com", "group/sub/app");
    let reply = |path: &str, url: &str| {
        serde_json::json!({
            "id": 42, "path_with_namespace": path, "web_url": url, "default_branch": "main"
        })
    };
    let project = parse_project(
        &reply("Group/Sub/App", "https://gitlab.example.com/Group/Sub/App/"),
        &remote,
    )
    .unwrap();
    assert_eq!(project.id, 42);
    assert_eq!(project.web_url, "https://gitlab.example.com/Group/Sub/App");
    assert_eq!(project.default_branch.as_deref(), Some("main"));
    // A relative URL root keeps its prefix.
    assert!(
        parse_project(
            &reply(
                "group/sub/app",
                "https://gitlab.example.com/gitlab/group/sub/app"
            ),
            &remote
        )
        .is_ok()
    );
    for (path, url) in [
        (
            "group/sub/other",
            "https://gitlab.example.com/group/sub/other",
        ),
        ("group/sub/app", "https://evil.test/group/sub/app"),
        (
            "group/sub/app",
            "https://gitlab.example.com@evil.test/group/sub/app",
        ),
        (
            "group/sub/app",
            "https://gitlab.example.com/group/sub/app?x=1",
        ),
        (
            "group/sub/app",
            "javascript:alert(1)//gitlab.example.com/group/sub/app",
        ),
        ("group/sub/app", "https://gitlab.example.com/other/app"),
    ] {
        assert!(
            matches!(
                parse_project(&reply(path, url), &remote),
                Err(Error::GitLabProject)
            ),
            "{path} {url}"
        );
    }
    assert!(
        parse_project(
            &serde_json::json!({"message": "404 Project Not Found"}),
            &remote
        )
        .is_err()
    );
}

#[test]
fn glab_auth_status_lines_name_the_signed_in_hosts() {
    let text = "gitlab.com\n  ✓ Logged in to gitlab.com as octo (/home/u/.config/glab-cli/config.yml)\n  ✓ Token: **************************\ngitlab.example.com\n  x gitlab.example.com: API call failed: 401\ncode.example.org:8443\n  ✓ Logged in to Code.Example.org:8443 as octo (GITLAB_TOKEN)\n  ✓ Logged in to gitlab.com as octo (again)\n  ✓ Logged in to evil host as x\n  ✓ Logged in to github.com as x\n";
    assert_eq!(
        probe::signed_in_hosts(text),
        ["gitlab.com", "code.example.org:8443"]
    );
    let many: String = (0..10)
        .map(|index| format!("Logged in to host{index}.test as a\n"))
        .collect();
    assert_eq!(probe::signed_in_hosts(&many).len(), 4);
}

#[cfg(unix)]
#[test]
fn glab_probe_keeps_hosts_that_answer_and_reports_signed_out() {
    use probe::Status;
    let deadline = || Instant::now() + Duration::from_secs(10);
    // glab reports on stderr and exits 1 when any host fails; the user reply
    // is answered only for gitlab.com.
    let fake = Fake::new(
        "glab",
        "case \"$1\" in\n  auth) printf '  ✓ Logged in to gitlab.com as octo (keyring)\\n  ✓ Logged in to gitlab.example.com as octo (keyring)\\n' >&2; exit 1;;\n  api) if [ \"$3\" = gitlab.com ]; then printf '{\"username\":\"octo\"}'; else echo 'glab: 401 Unauthorized' >&2; exit 1; fi;;\nesac",
    );
    match probe::glab(&fake.program, deadline()) {
        Status::SignedIn(account) => {
            assert_eq!(account.login, "octo");
            assert_eq!(&*account.hosts, ["gitlab.com".to_owned()]);
        }
        _ => panic!("expected a signed-in glab"),
    }
    let log = fake.log();
    assert_eq!(&log[..2], ["auth", "status"]);
    assert_eq!(&log[2..6], ["api", "--hostname", "gitlab.com", "user"]);
    let fake = Fake::new(
        "glab",
        "echo 'No GitLab instances are authenticated. Run glab auth login.' >&2\nexit 1",
    );
    assert!(matches!(
        probe::glab(&fake.program, deadline()),
        Status::SignedOut
    ));
    assert!(matches!(
        probe::glab(Path::new("/nonexistent/herdr-test/glab"), deadline()),
        Status::NotInstalled
    ));
}

#[cfg(unix)]
#[test]
fn glab_requests_map_failures_to_typed_errors() {
    use super::gitlab::Client;
    let fake = Fake::new(
        "glab",
        "case \"$4\" in\n  user) printf '[1,2]';;\n  limited) echo 'glab: 429 Too Many Requests' >&2; exit 1;;\n  *) echo 'glab: 401 Unauthorized' >&2; exit 1;;\nesac",
    );
    let client = Client {
        program: &fake.program,
        host: "gitlab.com",
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut cooldown = None;
    assert_eq!(
        client
            .get("test", "user", deadline, &|| false, &mut cooldown)
            .unwrap(),
        serde_json::json!([1, 2])
    );
    assert!(matches!(
        client.get("test", "limited", deadline, &|| false, &mut cooldown),
        Err(Error::CliRateLimit(Kind::GitLab))
    ));
    assert_eq!(cooldown, Some(cli::RATE_LIMIT_COOLDOWN));
    assert!(matches!(
        client.get("test", "projects/1", deadline, &|| false, &mut cooldown),
        Err(Error::CliSignedOut(Kind::GitLab))
    ));
    assert_eq!(
        Error::CliSignedOut(Kind::GitLab).to_string(),
        "glab is not signed in. Run `glab auth login` for this host."
    );
}
