#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
use super::probe::{HostPath, Request, Shell};
use super::{
    Host, Message, Reading, Usage, UsageConfig,
    model::{Account, Kind, Provider, Report, SESSION, Section, Severity, WEEK, Window, countdown},
    probe::{Exec, Probe, Response, json_field},
    providers::{claude, codex},
    registry,
    settings::ProviderSettings,
};
use crate::Error;
use std::time::{Duration, Instant, SystemTime};

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn provider(id: &str) -> Provider {
    registry::find(id).unwrap()
}

/// Trimmed from a live response: model windows only appear in `limits`.
const CLAUDE: &str = r#"{"five_hour":{"utilization":3.0,"resets_at":"2026-09-25T08:20:00.978246+00:00"},
"seven_day":{"utilization":15.0,"resets_at":"2026-09-29T16:59:59+00:00"},
"extra_usage":{"is_enabled":false},
"limits":[
 {"kind":"session","group":"session","percent":3,"resets_at":"2026-09-25T08:20:00+00:00","scope":null},
 {"kind":"weekly_all","group":"weekly","percent":15,"resets_at":"2026-09-29T16:59:59+00:00","scope":null},
 {"kind":"weekly_scoped","group":"weekly","percent":0,"resets_at":"2026-09-29T17:00:00+00:00",
  "scope":{"model":{"id":null,"display_name":"Fable"},"surface":null}},
 {"kind":"monthly_spend","percent":50,"resets_at":null}
],
"spend":{"used":{"amount_minor":1234,"currency":"EUR","exponent":2},
 "limit":{"amount_minor":5000,"currency":"EUR","exponent":2},"enabled":true},
"seven_day_breakdown":{"rows":[{"key":"claude_code","display_name":"Claude Code","percent":90},
 {"key":"chat","display_name":"Chats","percent":10},{"key":"other","display_name":"Other","percent":0}]}}"#;

#[test]
fn claude_reads_every_window_account_and_detail() {
    let report = claude::parse(
        CLAUDE,
        claude::SignIn {
            plan: Some("max".into()),
            tier: Some("default_claude_max_20x".into()),
            email: Some("me@example.com".into()),
        },
    )
    .unwrap();
    assert_eq!(report.provider, provider("claude"));
    let windows: Vec<_> = report
        .windows
        .iter()
        .map(|w| (w.kind.clone(), w.percent(), w.length))
        .collect();
    assert_eq!(
        windows,
        [
            (Kind::Session, 3, Some(SESSION)),
            (Kind::Weekly, 15, Some(WEEK)),
            (Kind::Named("Fable".into()), 0, Some(WEEK)),
        ]
    );
    assert_eq!(report.windows[0].resets_at, Some(at(1_790_324_400)));
    assert_eq!(
        report.account,
        Account {
            email: Some("me@example.com".into()),
            plan: Some("Max 20x".into()),
        }
    );
    assert_eq!(
        report.sections,
        [
            Section::Shares {
                title: "This week by surface".into(),
                shares: vec![("Claude Code".into(), 90.), ("Chats".into(), 10.)],
            },
            Section::Facts {
                title: "Extra usage".into(),
                facts: vec![("This month".into(), "12.34 EUR of 50.00 EUR".into())],
            },
        ]
    );
}

#[test]
fn claude_falls_back_to_the_fixed_windows() {
    let body = r#"{"five_hour":{"utilization":42.4,"resets_at":1790324400},
        "seven_day":{"utilization":150,"resets_at":null},"limits":null,
        "spend":{"enabled":false}}"#;
    let report = claude::parse(body, claude::SignIn::default()).unwrap();
    assert_eq!(report.windows[0].kind, Kind::Session);
    assert_eq!(report.windows[0].percent(), 42);
    // Clamped: a service rounding past its own limit still reads as full.
    assert_eq!(report.windows[1].percent(), 100);
    assert_eq!(report.windows[1].left(), 0);
    assert_eq!(report.account, Account::default());
}

#[test]
fn claude_plans_name_their_tier_multiple() {
    assert_eq!(
        claude::plan(Some("max"), Some("default_claude_max_20x")).as_deref(),
        Some("Max 20x")
    );
    assert_eq!(
        claude::plan(Some("pro"), Some("default_claude_ai")).as_deref(),
        Some("Pro")
    );
    assert_eq!(claude::plan(None, Some("default_claude_max_5x")), None);
}

/// Live shape: a Pro plan with only a weekly limit, in the primary slot.
const CODEX: &str = r#"{"email":"me@example.com","plan_type":"pro","rate_limit":{"allowed":true,
    "primary_window":{"used_percent":11,"limit_window_seconds":604800,"reset_after_seconds":472393,
    "reset_at":1790786634},"secondary_window":null},
    "code_review_rate_limit":{"primary_window":{"used_percent":4,"limit_window_seconds":604800,
    "reset_at":1790786634},"secondary_window":null},
    "credits":{"has_credits":false,"unlimited":false,"balance":"0"},
    "rate_limit_reset_credits":{"available_count":2,"applicable_available_count":0}}"#;

#[test]
fn codex_windows_are_known_by_length_not_slot() {
    let report = codex::parse(CODEX).unwrap();
    assert_eq!(report.provider, provider("codex"));
    assert_eq!(report.account.plan.as_deref(), Some("Pro"));
    assert_eq!(report.windows.len(), 1);
    assert_eq!(report.windows[0].kind, Kind::Weekly);
    assert_eq!(report.windows[0].resets_at, Some(at(1_790_786_634)));
    assert_eq!(report.sections.len(), 3);

    let report = codex::parse(
        r#"{"plan_type":"plus","rate_limit":{
        "primary_window":{"used_percent":70,"limit_window_seconds":1,"reset_at":1790000000000},
        "secondary_window":{"used_percent":90,"reset_at":null}}}"#,
    )
    .unwrap();
    assert_eq!(report.windows[0].kind, Kind::Session);
    assert_eq!(report.windows[0].resets_at, Some(at(1_790_000_000)));
    assert_eq!(report.windows[1].kind, Kind::Weekly);
}

#[test]
fn statuses_become_typed_errors_without_echoing_the_body() {
    let response = |status| Response {
        status,
        body: "secret@example.com".into(),
    };
    assert!(matches!(response(0).ok(), Err(Error::UsageConnect)));
    assert!(matches!(response(401).ok(), Err(Error::UsageRejected)));
    assert!(matches!(response(403).ok(), Err(Error::UsageRejected)));
    assert!(matches!(response(429).ok(), Err(Error::UsageRateLimited)));
    assert!(matches!(response(500).ok(), Err(Error::UsageStatus(500))));
    assert_eq!(response(204).ok().unwrap(), "secret@example.com");
    let error = codex::parse(r#"{"plan_type": "secret@example.com"#).unwrap_err();
    assert!(matches!(
        error,
        Error::UsageJson(serde_json::error::Category::Eof)
    ));
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn labels_count_down_in_the_two_coarsest_units() {
    assert_eq!(countdown(Duration::ZERO), "0m");
    assert_eq!(countdown(Duration::from_secs(47 * 60 - 30)), "47m");
    assert_eq!(countdown(Duration::from_secs(2 * 3600 + 53 * 60)), "2h 53m");
    assert_eq!(
        countdown(Duration::from_secs(4 * 86_400 + 11 * 3600 + 59)),
        "4d 11h"
    );
    let now = at(1_000_000);
    let session = Window::new(
        Kind::Session,
        2.4,
        Some(now + Duration::from_secs(10_380)),
        None,
    );
    assert_eq!(session.label(now), "2% used 2h 53m");
    assert_eq!(
        session.label(now + Duration::from_secs(20_000)),
        "2% used 0m"
    );
    let named = Window::new(Kind::Named("Fable".into()), 0., Some(now), None);
    assert_eq!(named.label(now), "0% used Fable");
    for (kind, suffix) in [
        (Kind::Session, "5h"),
        (Kind::Weekly, "wk"),
        (Kind::Monthly, "mo"),
    ] {
        assert_eq!(
            Window::new(kind, 1., None, None).label(now),
            format!("1% used {suffix}")
        );
    }
}

#[test]
fn pace_compares_use_with_an_even_spend() {
    let now = at(1_000_000);
    let window =
        |used, left: Duration| Window::new(Kind::Weekly, used, Some(now + left), Some(WEEK));
    let pace = window(20., WEEK / 2).pace(now).unwrap();
    assert_eq!(pace.describe(20.), "30% in reserve · Lasts until reset");
    let pace = window(50., WEEK * 3 / 4).pace(now).unwrap();
    assert_eq!(pace.runs_out, Some(WEEK / 4));
    assert_eq!(pace.describe(50.), "25% in deficit · Runs out in 1d 18h");
    assert_eq!(
        window(49.8, WEEK / 2).pace(now).unwrap().describe(49.8),
        "On pace · Lasts until reset"
    );
    assert_eq!(window(1., WEEK).pace(now), None);
    assert_eq!(window(1., WEEK / 2).pace(now + WEEK), None);
}

#[test]
fn severity_follows_the_share_used() {
    assert_eq!(Severity::from(59.9), Severity::Normal);
    assert_eq!(Severity::from(60.), Severity::Warning);
    assert_eq!(Severity::from(80.), Severity::Critical);
}

#[test]
fn every_provider_is_registered_once_with_its_icon() {
    let ids: Vec<_> = registry::all().map(|p| p.id()).collect();
    let mut unique = ids.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "duplicate provider id");
    assert_eq!(ids, ["codex", "claude", "grok"]);
    for provider in registry::all() {
        assert!(
            provider
                .id()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{}",
            provider.id()
        );
        let icon = provider.icon();
        assert!(
            super::icon(icon).is_some() || icon.starts_with("icons/agent-"),
            "{} has no icon at {icon}",
            provider.id()
        );
        for url in provider
            .service()
            .meta()
            .dashboard
            .into_iter()
            .chain(provider.service().meta().status_page)
        {
            assert!(url.starts_with("https://"), "{url}");
        }
        for setting in provider.service().meta().settings {
            assert!(
                !setting.help.trim().is_empty(),
                "{}.{}",
                provider.id(),
                setting.name
            );
        }
    }
}

#[test]
fn config_checks_known_providers_settings_and_tolerates_the_rest() {
    let parse = |text: &str| -> crate::Result<UsageConfig> {
        let config: UsageConfig = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    };
    let config = parse("show_providers = [\"claude\"]\nhide_providers = [\"codex\"]").unwrap();
    assert!(config.shown(provider("claude")));
    assert!(config.hidden(provider("codex")));
    assert!(config.show);
    // A config written for providers since removed, or for the retired
    // browser cookie import, still loads; the unknown ids match nothing.
    let config = parse(
        "show_providers = [\"cursor\", \"claude\"]\nhide_providers = [\"gemini\"]\n\
         browser_cookies = false\n[providers.openrouter]\napi_key = \"x\"\n\
         [providers.grok]\ntoken = \"y\"",
    )
    .unwrap();
    assert!(config.shown(provider("claude")));
    assert!(!config.shown(provider("codex")));
    assert!(registry::all().all(|provider| !config.hidden(provider)));
    assert!(config.settings(provider("grok")).is_some());
    // A known provider's settings are still checked.
    assert!(matches!(
        parse("[providers.claude]\napi_key = \"x\""),
        Err(Error::UnknownUsageSetting { provider, setting })
            if provider == "claude" && setting == "api_key"
    ));
    assert!(parse("unknown = 1").is_err());
}

#[test]
fn json_fields_follow_paths_through_objects_and_arrays() {
    let text = r#"{"a":{"b":[{"c":"x"},{"c":7}]},"t":true,"n":null}"#;
    assert_eq!(
        json_field(text, &["a", "b", "0", "c"]).as_deref(),
        Some("x")
    );
    assert_eq!(
        json_field(text, &["a", "b", "1", "c"]).as_deref(),
        Some("7")
    );
    assert_eq!(json_field(text, &["t"]).as_deref(), Some("true"));
    assert_eq!(json_field(text, &["n"]), None);
    assert_eq!(json_field(text, &["a"]), None);
    assert_eq!(json_field("not json", &["a"]), None);
}

/// A local `sh` standing in for the remote host: its home holds agent
/// sign-ins, and a fake `curl` on its PATH records what it was given.
#[cfg(unix)]
struct FakeHost {
    root: tempfile::TempDir,
}

#[cfg(unix)]
impl FakeHost {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(
            home.join(".codex/auth.json"),
            "{\n  \"tokens\": {\n    \"id_token\": \"id-fixture\",\n    \"access_token\": \"codex-fixture-token\",\n    \"account_id\": \"acct-fixture\"\n  }\n}\n",
        )
        .unwrap();
        let log = root.path().join("log");
        // Records its arguments and the -K config it read from fd 3, then
        // answers a fixed body; `-w` output is emulated.
        std::fs::write(
            bin.join("curl"),
            format!(
                "#!/bin/sh\nprintf 'args: %s\\n' \"$*\" >> '{log}'\ncat <&3 >> '{log}'\nprintf '{{\"token\":\"minted-secret\",\"ok\":true}}'\ncase \"$*\" in *herdr-status*) printf '\\n@@herdr-status 200';; esac\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn shell(&self) -> Shell {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-s")
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        Shell::start(command).unwrap()
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.path().join("log")).unwrap_or_default()
    }
}

#[cfg(unix)]
#[test]
fn remote_secrets_stay_on_the_host() {
    let host = FakeHost::new();
    let mut exec = Exec::Remote(host.shell());
    let codex = provider("codex");
    let mut probe = Probe::new(&mut exec, codex, None);
    assert!(probe.is_remote());
    let auth = probe
        .file(&HostPath::env_or("CODEX_HOME", ".codex", "auth.json"))
        .unwrap();
    assert!(format!("{auth:?}").contains("remote"));
    let token = probe.field(&auth, &["tokens", "access_token"]).unwrap();
    assert_eq!(
        probe.text(&auth, &["tokens", "account_id"]).as_deref(),
        Some("acct-fixture")
    );
    assert!(probe.file(&HostPath::home("missing.json")).is_none());

    let response = probe
        .http(
            Request::get("https://example.com/usage?q=a+b")
                .bearer(&token)
                .header("X-Plain", "it's $HOME `x` \\ \"q\""),
        )
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "{\"token\":\"minted-secret\",\"ok\":true}");

    let log = host.log();
    for line in log.lines().filter(|line| line.starts_with("args: ")) {
        assert!(!line.contains("fixture"), "{line}");
    }
    assert!(log.contains("header = \"Authorization: Bearer codex-fixture-token\""));
    // Literal text survives both the shell and curl's config quoting.
    assert!(
        log.contains(r#"header = "X-Plain: it's $HOME `x` \\ \"q\"""#),
        "{log}"
    );
    assert!(!log.contains("id-fixture"));
}

#[cfg(unix)]
#[test]
fn remote_steps_report_failure_and_keep_the_session() {
    let host = FakeHost::new();
    let mut shell = host.shell();
    let output = shell
        .run("printf 'a\\nb'; false", Duration::from_secs(5))
        .unwrap();
    assert!(!output.success);
    assert_eq!(output.stdout, "a\nb");
    let output = shell.run("printf ok", Duration::from_secs(5)).unwrap();
    assert!(output.success);
    assert_eq!(output.stdout, "ok");
    assert!(matches!(
        shell.run("sleep 5", Duration::from_millis(200)),
        Err(Error::UsageTimeout)
    ));
    // A step that overran leaves the session unusable rather than out of step.
    assert!(shell.run("true", Duration::from_secs(5)).is_err());
}

#[test]
fn settings_come_from_this_machines_config() {
    let settings = ProviderSettings::default().with("token", "config-token");
    let mut exec = Exec::Local;
    let probe = Probe::new(&mut exec, provider("grok"), Some(&settings));
    let token = probe.setting("token").unwrap();
    assert_eq!(format!("{token:?}"), "Secret(here)");
    assert!(probe.setting("missing").is_none());
}

fn report(provider: Provider, used: f64) -> Report {
    Report::new(
        provider,
        Account::default(),
        vec![Window::new(Kind::Session, used, None, None)],
    )
}

fn begin(usage: &mut Usage, host: &Host, now: Instant) {
    usage.host = Some(host.clone());
    usage.begin(host.clone(), now);
}

#[test]
fn a_failed_refresh_keeps_the_last_numbers_and_says_why() {
    let now = Instant::now();
    let (claude, codex) = (provider("claude"), provider("codex"));
    let local = Host::Local;
    let mut usage = Usage::default();
    assert!(usage.poll(Some(local.clone()), &UsageConfig::default(), 0, false, now));
    assert!(
        usage.current().is_none(),
        "an inactive window reads nothing"
    );

    begin(&mut usage, &local, now);
    assert!(usage.busy());
    usage.apply(
        Message::Reading(local.clone(), claude, Ok(report(claude, 20.))),
        now,
    );
    usage.apply(Message::Done(local.clone(), Ok(())), now);
    assert!(!usage.busy());

    begin(&mut usage, &local, now);
    usage.apply(
        Message::Reading(local.clone(), claude, Err(Error::UsageRateLimited)),
        now,
    );
    usage.apply(
        Message::Reading(local.clone(), codex, Err(Error::UsageRejected)),
        now,
    );
    usage.apply(Message::Done(local.clone(), Ok(())), now);
    let entry = usage.current().unwrap();
    assert_eq!(
        entry.readings,
        [
            Reading {
                provider: codex,
                report: None,
                error: Some(Error::UsageRejected.to_string()),
            },
            Reading {
                provider: claude,
                report: Some(report(claude, 20.)),
                error: Some(Error::UsageRateLimited.to_string()),
            },
        ],
        "registry order, whatever order answers came in"
    );
    assert_eq!(entry.due, Some(now + super::RATE_LIMITED));

    begin(&mut usage, &local, now);
    usage.apply(
        Message::Done(local.clone(), Err(Error::UsageUnreachable)),
        now,
    );
    let entry = usage.current().unwrap();
    assert_eq!(
        entry.readings.len(),
        2,
        "an unreachable host keeps its readings"
    );
    assert_eq!(entry.due, Some(now + super::ERROR_BACKOFF));

    begin(&mut usage, &local, now);
    usage.apply(
        Message::Reading(local.clone(), codex, Ok(report(codex, 5.))),
        now,
    );
    usage.apply(Message::Done(local.clone(), Ok(())), now);
    let entry = usage.current().unwrap();
    assert_eq!(
        entry.readings.len(),
        1,
        "a provider that went silent is dropped"
    );
    assert_eq!(entry.readings[0].provider, codex);
}

#[test]
fn each_host_keeps_its_own_answer_within_a_bound() {
    let now = Instant::now();
    let claude = provider("claude");
    let mut usage = Usage::default();
    let remote = Host::Ssh("me@box".into());
    for host in [Host::Local, remote.clone()] {
        begin(&mut usage, &host, now);
        usage.apply(
            Message::Reading(host.clone(), claude, Ok(report(claude, 1.))),
            now,
        );
        usage.apply(Message::Done(host, Ok(())), now);
    }
    let config = UsageConfig::default();
    usage.poll(Some(Host::Local), &config, 0, false, now);
    assert_eq!(usage.current().unwrap().readings.len(), 1);
    usage.poll(None, &config, 0, false, now);
    assert!(usage.current().is_none());
    usage.poll(Some(remote.clone()), &config, 0, false, now);
    for index in 0..super::HOST_LIMIT * 2 {
        usage.begin(Host::Ssh(format!("host-{index}")), now);
    }
    usage.busy = None;
    assert!(usage.entries.len() <= super::HOST_LIMIT);
    assert!(
        usage.current().is_some(),
        "the shown host survives trimming"
    );
    // A new config makes every host due.
    usage.poll(Some(remote.clone()), &config, 1, false, now);
    assert!(usage.entries.values().all(|entry| entry.due == Some(now)));
}

#[test]
fn manual_refresh_is_spaced() {
    let now = Instant::now();
    let mut usage = Usage::default();
    begin(&mut usage, &Host::Local, now);
    usage.apply(Message::Done(Host::Local, Ok(())), now);
    usage.refresh(now + Duration::from_secs(1));
    assert_eq!(usage.current().unwrap().due, Some(now + super::REFRESH));
    usage.refresh(now + super::MANUAL_SPACING);
    assert_eq!(
        usage.current().unwrap().due,
        Some(now + super::MANUAL_SPACING)
    );
}

/// Regenerate with `HERDR_BLESS_EXAMPLE=1 cargo test example_config_documents`.
#[test]
fn example_config_documents_every_provider_setting() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config-gpui.example.toml");
    // A Windows checkout may carry CRLF line endings.
    let text = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
    let expected = super::settings::example_docs();
    if std::env::var_os("HERDR_BLESS_EXAMPLE").is_some() {
        let updated = match super::settings::docs_in(&text) {
            Some(current) => text.replace(current, &expected),
            None => format!(
                "{}\n{expected}",
                text.trim_end_matches('\n').to_owned() + "\n"
            ),
        };
        std::fs::write(path, updated).unwrap();
        return;
    }
    assert_eq!(
        super::settings::docs_in(&text),
        Some(expected.as_str()),
        "config-gpui.example.toml is stale; rerun with HERDR_BLESS_EXAMPLE=1"
    );
}

/// Reads this machine's real sign-ins and prints what each provider found:
/// `cargo test -p herdr-gpui live_local_usage -- --ignored --nocapture`.
/// Prints windows and typed errors only, never credentials.
#[test]
#[ignore = "reads this machine's agent sign-ins and calls their services"]
fn live_local_usage() {
    super::read(&Host::Local, &UsageConfig::default(), |provider, report| {
        match report {
            Ok(report) => println!(
                "{}: plan {:?}, windows {:?}",
                provider.id(),
                report.account.plan,
                report
                    .windows
                    .iter()
                    .map(|w| format!("{} {}%", w.kind.title(), w.percent()))
                    .collect::<Vec<_>>()
            ),
            Err(error) => println!("{}: error {error:?}", provider.id()),
        }
        true
    })
    .unwrap();
}

#[test]
fn the_status_bar_shows_the_two_closest_to_a_limit() {
    let reading = |id: &str, used: Option<f64>| Reading {
        provider: provider(id),
        report: used.map(|used| report(provider(id), used)),
        error: None,
    };
    let ids = |entry: &super::Entry, limit| {
        entry
            .headline(limit)
            .iter()
            .map(|reading| reading.provider.id())
            .collect::<Vec<_>>()
    };
    let entry = super::Entry {
        readings: vec![
            reading("codex", Some(12.)),
            reading("claude", Some(32.)),
            reading("grok", Some(12.)),
        ],
        ..Default::default()
    };
    assert_eq!(ids(&entry, super::HEADLINE), ["claude", "codex"]);
    // Ties keep the registry order.
    assert_eq!(ids(&entry, 10), ["claude", "codex", "grok"]);

    // No report yet, or an answer without windows, has nothing to show.
    let entry = super::Entry {
        readings: vec![
            reading("codex", None),
            reading("claude", Some(5.)),
            Reading {
                provider: provider("grok"),
                report: Some(Report::new(provider("grok"), Account::default(), vec![])),
                error: None,
            },
        ],
        ..Default::default()
    };
    assert_eq!(ids(&entry, 10), ["claude"]);
}

#[test]
fn panel_tabs_leave_out_sign_ins_with_nothing_to_show() {
    let with = |id: &str| Reading {
        provider: provider(id),
        report: Some(report(provider(id), 5.)),
        error: None,
    };
    let without = |id: &str| Reading {
        provider: provider(id),
        report: None,
        error: Some(Error::UsageNotSignedIn.to_string()),
    };
    let entry = super::Entry {
        readings: vec![with("codex"), without("claude"), without("grok")],
        ..Default::default()
    };
    let tabs = |config: &UsageConfig| {
        entry
            .tabs(config)
            .iter()
            .map(|reading| reading.provider.id())
            .collect::<Vec<_>>()
    };
    assert_eq!(tabs(&UsageConfig::default()), ["codex"]);
    // Asked for by the config: shown, so its panel can say what to set up.
    let asked: UsageConfig = toml::from_str("show_providers = [\"grok\"]").unwrap();
    assert_eq!(tabs(&asked), ["codex", "grok"]);
}
