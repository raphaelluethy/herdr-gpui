//! The Herdr daemon's `[keys]` table, read so the GUI answers the same
//! shortcuts, prefix chords included, as the TUI. Herdr owns that file and
//! reports its own mistakes, so anything this client cannot express (an
//! unparseable entry, a `hyper` modifier, an action with no GUI command) is
//! skipped rather than turned into a GUI config error.

use crate::controls::Command;
use gpui::{Keystroke, Modifiers};

/// Herdr's own fallback when `prefix` is missing or unparseable.
const DEFAULT_PREFIX: &str = "ctrl+b";

/// A daemon `[keys]` entry can hold a list, but never an unbounded one.
const MAX_ENTRIES: usize = 16;

/// What a daemon action runs here.
#[derive(Clone, Copy)]
enum Target {
    Command(Command),
    /// `switch_tab`: each digit 1-9 it binds selects that tab.
    TabNumber,
}

/// Daemon actions with a GUI equivalent, each with Herdr's default binding.
/// Actions missing here (renames, swaps, resize and copy modes, detach, ...)
/// have no GUI command, so the TUI keeps them to itself.
const ACTIONS: &[(&str, Target, &str)] = &[
    ("help", Target::Command(Command::Keybinds), "prefix+?"),
    ("settings", Target::Command(Command::Settings), "prefix+s"),
    (
        "new_workspace",
        Target::Command(Command::Workspace),
        "prefix+shift+n",
    ),
    (
        "new_worktree",
        Target::Command(Command::NewWorktree),
        "prefix+shift+g",
    ),
    (
        "workspace_picker",
        Target::Command(Command::WorkspacePicker),
        "prefix+w",
    ),
    (
        "goto",
        Target::Command(Command::WorkspacePicker),
        "prefix+g",
    ),
    (
        "open_notification_target",
        Target::Command(Command::OpenNotificationTarget),
        "prefix+o",
    ),
    ("new_tab", Target::Command(Command::Tab), "prefix+c"),
    (
        "previous_tab",
        Target::Command(Command::PreviousTab),
        "prefix+p",
    ),
    ("next_tab", Target::Command(Command::NextTab), "prefix+n"),
    ("switch_tab", Target::TabNumber, "prefix+1..9"),
    (
        "close_tab",
        Target::Command(Command::CloseTab),
        "prefix+shift+x",
    ),
    ("clear_pane", Target::Command(Command::ClearPane), ""),
    (
        "focus_pane_left",
        Target::Command(Command::FocusLeft),
        "prefix+h",
    ),
    (
        "focus_pane_down",
        Target::Command(Command::FocusDown),
        "prefix+j",
    ),
    (
        "focus_pane_up",
        Target::Command(Command::FocusUp),
        "prefix+k",
    ),
    (
        "focus_pane_right",
        Target::Command(Command::FocusRight),
        "prefix+l",
    ),
    (
        "cycle_pane_next",
        Target::Command(Command::NextPane),
        "prefix+tab",
    ),
    (
        "cycle_pane_previous",
        Target::Command(Command::PreviousPane),
        "prefix+shift+tab",
    ),
    (
        "split_vertical",
        Target::Command(Command::SplitRight),
        "prefix+v",
    ),
    (
        "split_horizontal",
        Target::Command(Command::SplitDown),
        "prefix+minus",
    ),
    (
        "close_pane",
        Target::Command(Command::ClosePane),
        "prefix+x",
    ),
    ("zoom", Target::Command(Command::Zoom), "prefix+z"),
    (
        "toggle_sidebar",
        Target::Command(Command::ToggleSidebar),
        "prefix+b",
    ),
];

/// How a daemon binding is typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// The keystroke alone runs the command.
    Direct(Keystroke),
    /// The prefix, then this keystroke.
    Prefixed(Keystroke),
}

/// The daemon's bindings for GUI commands, in the order its table lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonKeys {
    pub(super) prefix: Keystroke,
    pub(super) bindings: Vec<(Command, Trigger)>,
}

impl Default for DaemonKeys {
    /// Herdr's defaults, which also apply when it has no config file.
    fn default() -> Self {
        Self::from_table(None)
    }
}

impl DaemonKeys {
    /// Reads the daemon config's `[keys]` table; each action it leaves out,
    /// or gives a value of the wrong type, keeps Herdr's default.
    pub(crate) fn from_table(keys: Option<&toml::Table>) -> Self {
        let prefix = keys
            .and_then(|keys| keys.get("prefix"))
            .and_then(toml::Value::as_str)
            .and_then(keystroke)
            .or_else(|| keystroke(DEFAULT_PREFIX))
            .unwrap_or_default();
        let mut bindings = Vec::new();
        for &(name, target, default) in ACTIONS {
            let value = keys.and_then(|keys| {
                keys.get(name)
                    // Herdr still accepts zoom's old name.
                    .or_else(|| (name == "zoom").then(|| keys.get("fullscreen")).flatten())
            });
            let entries: Vec<&str> = match value {
                Some(toml::Value::String(entry)) => vec![entry.as_str()],
                Some(toml::Value::Array(entries)) if entries.iter().all(toml::Value::is_str) => {
                    entries.iter().filter_map(toml::Value::as_str).collect()
                }
                _ => vec![default],
            };
            for entry in entries.into_iter().take(MAX_ENTRIES) {
                for (index, trigger) in triggers(entry) {
                    let command = match (target, index) {
                        (Target::Command(command), _) => command,
                        (Target::TabNumber, Some(number)) => Command::TabNumber(number),
                        (Target::TabNumber, None) => continue,
                    };
                    bindings.push((command, trigger));
                }
            }
        }
        Self { prefix, bindings }
    }
}

/// Each binding one entry spells, with the digit it types, if any. `1..9`
/// stands for the nine digit keys; an unparseable entry yields nothing.
fn triggers(entry: &str) -> Vec<(Option<u8>, Trigger)> {
    let entry = entry.trim();
    let (prefixed, body) = match entry.strip_prefix("prefix+") {
        Some(body) => (true, body),
        None => (false, entry),
    };
    let trigger = |keystroke| {
        if prefixed {
            Trigger::Prefixed(keystroke)
        } else {
            Trigger::Direct(keystroke)
        }
    };
    if body.split('+').any(|part| part.trim() == "1..9") {
        return (1..=9)
            .filter_map(|digit| {
                let keystroke = keystroke(&body.replace("1..9", &digit.to_string()))?;
                Some((Some(digit), trigger(keystroke)))
            })
            .collect();
    }
    let Some(keystroke) = keystroke(body) else {
        return Vec::new();
    };
    let digit = keystroke
        .key
        .parse::<u8>()
        .ok()
        .filter(|digit| (1..=9).contains(digit));
    vec![(digit, trigger(keystroke))]
}

/// One daemon keystroke, `+`-separated as Herdr writes it, in GPUI's terms.
/// Herdr's `hyper` modifier has no GPUI equivalent, so it matches nothing.
fn keystroke(text: &str) -> Option<Keystroke> {
    let mut modifiers = Modifiers::default();
    let mut key = None;
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            return None;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.control = true,
            "shift" => modifiers.shift = true,
            "alt" | "option" | "meta" => modifiers.alt = true,
            "cmd" | "command" | "super" => modifiers.platform = true,
            "hyper" => return None,
            _ if key.is_none() => key = Some(part),
            _ => return None,
        }
    }
    let key = key?;
    let named = match key.to_ascii_lowercase().as_str() {
        "space" => "space",
        "enter" | "return" => "enter",
        "esc" | "escape" => "escape",
        "tab" => "tab",
        "backspace" | "bs" => "backspace",
        "left" => "left",
        "right" => "right",
        "up" => "up",
        "down" => "down",
        "minus" => "-",
        "comma" => ",",
        "period" => ".",
        "slash" => "/",
        "backslash" => "\\",
        "quote" => "'",
        "double_quote" | "double-quote" => "\"",
        "semicolon" => ";",
        "colon" => ":",
        "percent" => "%",
        "ampersand" => "&",
        "backtick" => "`",
        "plus" => "+",
        _ => "",
    };
    let mut chars = key.chars();
    let key = match (chars.next(), chars.next()) {
        _ if !named.is_empty() => named.to_owned(),
        (Some(single), None) if single.is_ascii_uppercase() => {
            modifiers.shift = true;
            single.to_ascii_lowercase().to_string()
        }
        (Some(single), None) => single.to_string(),
        _ => {
            let lower = key.to_ascii_lowercase();
            let number: u8 = lower.strip_prefix('f')?.parse().ok()?;
            if !(1..=35).contains(&number) {
                return None;
            }
            lower
        }
    };
    Some(Keystroke {
        modifiers,
        key,
        key_char: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn keys(text: &str) -> DaemonKeys {
        let table: toml::Table = text.parse().unwrap();
        DaemonKeys::from_table(table.get("keys").and_then(toml::Value::as_table))
    }

    fn parsed(text: &str) -> Keystroke {
        Keystroke::parse(text).unwrap()
    }

    fn prefixed(key: &str) -> Trigger {
        Trigger::Prefixed(parsed(key))
    }

    fn bound(keys: &DaemonKeys, command: Command) -> Vec<Trigger> {
        keys.bindings
            .iter()
            .filter(|(bound, _)| *bound == command)
            .map(|(_, trigger)| trigger.clone())
            .collect()
    }

    #[test]
    fn keystrokes_translate_herdr_spelling() {
        for (herdr, gpui) in [
            ("ctrl+b", "ctrl-b"),
            ("Control+Shift+N", "ctrl-shift-n"),
            ("N", "shift-n"),
            ("option+1", "alt-1"),
            ("meta+x", "alt-x"),
            ("cmd+t", "cmd-t"),
            ("super+t", "cmd-t"),
            ("minus", "-"),
            ("\\", "\\"),
            ("backslash", "\\"),
            ("?", "?"),
            ("[", "["),
            ("shift+tab", "shift-tab"),
            ("esc", "escape"),
            ("return", "enter"),
            ("space", "space"),
            ("f12", "f12"),
            ("f", "f"),
        ] {
            assert_eq!(keystroke(herdr), Some(parsed(gpui)), "{herdr}");
        }
        for invalid in [
            "", "ctrl+", "ctrl++b", "a+b", "hyper+x", "f0", "f99", "nope",
        ] {
            assert_eq!(keystroke(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn defaults_match_herdr() {
        let keys = DaemonKeys::default();
        assert_eq!(keys.prefix, parsed("ctrl-b"));
        assert_eq!(bound(&keys, Command::SplitRight), [prefixed("v")]);
        assert_eq!(bound(&keys, Command::SplitDown), [prefixed("-")]);
        assert_eq!(bound(&keys, Command::Keybinds), [prefixed("?")]);
        assert_eq!(bound(&keys, Command::Workspace), [prefixed("shift-n")]);
        assert_eq!(
            bound(&keys, Command::WorkspacePicker),
            [prefixed("w"), prefixed("g")]
        );
        assert_eq!(bound(&keys, Command::PreviousPane), [prefixed("shift-tab")]);
        for digit in 1..=9 {
            let key = prefixed(&digit.to_string());
            assert_eq!(bound(&keys, Command::TabNumber(digit)), [key]);
        }
        assert!(bound(&keys, Command::ClearPane).is_empty());
        assert_eq!(keys, DaemonKeys::from_table(Some(&toml::Table::new())));
    }

    #[test]
    fn the_issue_example_reads_as_written() {
        let keys = keys(
            r#"
            [keys]
            prefix = "ctrl+a"
            split_vertical = ["prefix+v", "prefix+\\"]
            focus_pane_left = ["prefix+h", "prefix+left"]
            switch_tab = ["prefix+1..9", "alt+1..9"]
            "#,
        );
        assert_eq!(keys.prefix, parsed("ctrl-a"));
        assert_eq!(
            bound(&keys, Command::SplitRight),
            [
                Trigger::Prefixed(parsed("v")),
                Trigger::Prefixed(parsed("\\"))
            ]
        );
        assert_eq!(
            bound(&keys, Command::FocusLeft),
            [
                Trigger::Prefixed(parsed("h")),
                Trigger::Prefixed(parsed("left"))
            ]
        );
        assert_eq!(
            bound(&keys, Command::TabNumber(3)),
            [
                Trigger::Prefixed(parsed("3")),
                Trigger::Direct(parsed("alt-3"))
            ]
        );
        // Actions the file leaves alone keep Herdr's defaults.
        assert_eq!(bound(&keys, Command::Tab), [Trigger::Prefixed(parsed("c"))]);
    }

    #[test]
    fn unusable_entries_are_skipped_not_fatal() {
        let keys = keys(
            r#"
            [keys]
            prefix = "hyper+a"
            new_tab = ""
            next_tab = ["prefix+hyper+n", "prefix+n"]
            close_tab = 5
            fullscreen = "prefix+f"
            switch_tab = ["prefix+0", "prefix+4", "prefix+x"]
            reload_config = "prefix+r"
            "#,
        );
        // Herdr falls back to ctrl+b for a prefix it cannot parse.
        assert_eq!(keys.prefix, parsed("ctrl-b"));
        assert!(bound(&keys, Command::Tab).is_empty());
        assert_eq!(
            bound(&keys, Command::NextTab),
            [Trigger::Prefixed(parsed("n"))]
        );
        assert_eq!(
            bound(&keys, Command::CloseTab),
            [Trigger::Prefixed(parsed("shift-x"))]
        );
        assert_eq!(
            bound(&keys, Command::Zoom),
            [Trigger::Prefixed(parsed("f"))]
        );
        assert_eq!(
            bound(&keys, Command::TabNumber(4)),
            [Trigger::Prefixed(parsed("4"))]
        );
        assert!(bound(&keys, Command::TabNumber(1)).is_empty());
    }
}
