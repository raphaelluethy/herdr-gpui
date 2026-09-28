# Usage providers

The status bar shows plan usage for every AI service signed in on the
selected host, and clicking one opens its panel. Each service is a
*provider*: one module in `crates/herdr-gpui/src/usage/providers/` with a
unit struct implementing `Service`, listed once in
`crates/herdr-gpui/src/usage/registry.rs`. The built-in providers are
Codex, Claude, and Grok. They are ported from
[CodexBar](https://github.com/steipete/CodexBar) (MIT), whose
`docs/<id>.md` and `Sources/CodexBarCore/Providers/<Name>/` document each
service's sign-in, endpoints, and response shapes.

Detection, local and remote fetching, caching, refresh timing, the status
bar, and the panel all work from the trait. A provider only says how to
find its sign-in, how to ask its service, and how to read the answer.

## The trait

```rust
pub(crate) trait Service: Sync {
    fn meta(&self) -> &'static Meta;
    fn fetch(&self, probe: &mut Probe) -> Option<Result<Report>>;
    fn render(&self, report: &Report, ui: &Ui, cx: &App) -> AnyElement {
        ui.standard(report)
    }
}
```

What a provider is lives in one `static`:

```rust
static META: Meta = Meta::new("grok", "Grok")
    .dashboard("https://grok.com/?_s=usage")
    .status_page("https://status.x.ai")
    .settings(&[Setting::new("token", &["GROK_OAUTH_TOKEN"], "…")]);

impl Service for Grok {
    fn meta(&self) -> &'static Meta {
        &META
    }

    fn fetch(&self, probe: &mut Probe) -> Option<Result<Report>> {
        let token = probe.setting("token")?;
        Some(probe.body(Request::get(URL).bearer(&token)).and_then(|body| parse(&body)))
    }
}
```

`id` is lowercase ASCII and stable (config tables use it). The icon is
`assets/icons/providers/<id>.svg` when that exists and is listed in
`usage/icons.rs`; `.icon(path)` sets another embedded SVG. `.dashboard(url)`
and `.status_page(url)` must be HTTPS.

`fetch` returns `None` when the probed host has no sign-in and the config
sets none: the provider is then left out, unless the user lists it in
`[usage] show_providers`, in which case the panel shows its settings as a
setup guide. It returns `Some(Err(..))` when a sign-in exists but reading
failed, and `Some(Ok(report))` otherwise. Never block without a bound:
every probe call has a timeout.

## Settings

A provider declares every config value it reads. Users set them in
`config-gpui.local.toml`:

```toml
[usage.providers.grok]
token = "…"
```

```rust
.settings(&[Setting::new(
    "token",
    &["GROK_OAUTH_TOKEN"],
    "A SuperGrok bearer token, only needed when the Grok CLI is not signed in …",
)])
```

- `name` is the key in the table. Use `api_key`, `base_url`, `token`, and
  for anything else a short snake_case name.
- `env` lists environment variables read when the config has no value, in
  CodexBar's names. Apps launched from the Dock see few variables, so the
  config is the dependable place.
- `help` tells the user what the value is and exactly how to get it. The
  panel shows this text when the provider is listed but not signed in.
- Unknown setting names of a built-in provider are rejected, so every name
  a provider reads must be declared. Provider ids that are not built in,
  such as those of providers since removed, are logged and ignored, so an
  older config keeps loading.

## Probe

`Probe` is the only way a provider touches the host, the config, or the
network. The same calls work locally and on a remote host, where each runs
in one SSH shell session. A `Secret` read on a remote host stays there as a
shell variable; requests using it run there with `curl`. Never turn a
secret into a `String` except through `text` for values that are not
secret (emails, plan names, ids shown to the user).

| Call | Returns | Use |
| --- | --- | --- |
| `setting(name)` | `Option<Secret>` | A declared setting from config or its env vars (this machine). |
| `env(name)` | `Option<Secret>` | An environment variable on the probed host. |
| `file(&HostPath)` | `Option<Secret>` | A whole file on the host, e.g. a credentials JSON; trailing newlines dropped. |
| `file_text(&HostPath, &[key])` | `Option<String>` | A non-secret JSON field of a file (an email) without bringing the file back. |
| `keychain(service, account)` | `Option<Secret>` | A macOS keychain password on the host (`security -w`). |
| `field(&secret, &[key])` | `Option<Secret>` | A string/number at a JSON path inside a secret (array steps are indices as strings). |
| `text(&secret, &[key])` | `Option<String>` | A non-secret JSON field revealed, e.g. a plan name. |
| `body(Request)` | `Result<String>` | The body of a successful answer; failures become typed usage errors. Most providers need only this. |
| `http(Request)` | `Result<Response>` | The whole answer, for a provider that reads the status itself; runs where its secrets are. |
| `is_remote()`, `is_macos()` | `bool` | Where the probe runs. |

`HostPath::home(".codex/auth.json")` is `~/.codex/auth.json` on the host;
`HostPath::env_or("CODEX_HOME", ".codex", "auth.json")` honors the
variable.

`Request::get(url)` with `.header(name, value)`, `.bearer(&secret)`,
`.secret_header(name, prefix, &secret)`, and `.timeout(duration)`. A
request may not mix this machine's settings with secrets read on a remote
host.

`Response { status, body }`: `response.ok()?` maps 401/403 to
`UsageRejected`, 429 to `UsageRateLimited`, other failures to typed errors,
and returns the body; `response.json::<T>()?` also parses it. Parse with
serde structs; `service::json(body)` maps parse errors without echoing the
body, and `service::invalid()` is the error for a response that lacks what
the service documents. `service::Timestamp` accepts seconds, milliseconds,
numeric strings, and RFC 3339.

Errors: use `crate::Error::Usage*` variants (`UsageRejected`,
`UsageRateLimited`, `UsageStatus`, `UsageJson`, `UsageConnect`). Never
build an error from response text.

## Report

```rust
Report::new(Provider(&MyService), Account { email, plan }, windows)
    .with_sections([Section::Facts { title, facts }, Section::Shares { .. }, Section::Limit(window)])
```

- `Window::new(kind, used_percent, resets_at, length)`. `Kind` is
  `Session` (5 h), `Weekly`, `Monthly`, or `Named(String)` for a window the
  service names itself. `length` is the window's length (`SESSION`, `WEEK`,
  `MONTH`, or the service's own), so the panel can show the pace. The
  status bar shows every window as `N% used <time to reset>`.
- Prefer windows; use sections for anything else worth seeing. A report
  without windows waits in the panel rather than the status bar.
- `Provider(&MyService)` builds the report's provider from the unit struct.

## Rendering

The default `render` draws windows and sections. A provider may override
`render` and compose `Ui` pieces: `ui.limit(&window)`,
`ui.facts(title, &facts)`, `ui.shares(title, &shares)`,
`ui.bar(fill, used, None)`, `ui.block()`, `ui.heading(..)`, `ui.rule()`,
with `ui.theme`, `ui.small()`, `ui.muted()`.

## Tests

Keep parsing in a pure `fn parse(body: &str, ..) -> Result<Report>` so it
can be tested without a network, and test it against a fixture response
shaped like the real one (take it from CodexBar's tests or docs), checking
the windows, sections, and account it produces. Claude and Codex are
tested in `usage/tests.rs`; Grok in its own module.

## What is not supported

Sources that need more than the probe offers are left out and noted in the
module doc: browser cookies or other browser storage, local SQLite
databases, WebView sessions, gRPC-web, POST requests, running CLIs on the
host, interactive logins, and signing requests (AWS SigV4, Google service
accounts).
