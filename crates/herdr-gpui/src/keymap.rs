//! The keystrokes bound to each catalog command: the defaults in
//! `controls::COMMANDS`, then the daemon config's `[keys]` table (prefix
//! chords included), with the GUI config file's `[keybindings]` table layered
//! on top. The palette, keybindings page, menu bar, GPUI keymap, and prefix
//! mode all read this one resolved answer.

mod daemon;

pub(crate) use daemon::DaemonKeys;

use crate::{
    Error, Result,
    controls::{COMMANDS, Command},
};
use daemon::Trigger;
use gpui::{KeybindingKeystroke, Keystroke, Modifiers};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// A command may carry an alias or two, never an unbounded list.
const MAX_KEYSTROKES: usize = 8;

/// Keystrokes the GUI config binds, with the command holding each. Spelling
/// differences still name the same keystroke.
type Claimed = HashMap<(Modifiers, String), &'static str>;

fn identity(keystroke: &Keystroke) -> (Modifiers, String) {
    (keystroke.modifiers, keystroke.key.clone())
}

/// One entry of the config's `[keybindings]` table: a keystroke, or a list of
/// them. An empty string or list leaves the command unbound.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Binding {
    One(String),
    Many(Vec<String>),
}

impl Binding {
    fn keystrokes(&self) -> impl Iterator<Item = &str> {
        let keystrokes = match self {
            Self::One(keystroke) => std::slice::from_ref(keystroke),
            Self::Many(keystrokes) => keystrokes.as_slice(),
        };
        keystrokes
            .iter()
            .map(|keystroke| keystroke.trim())
            .filter(|keystroke| !keystroke.is_empty())
    }
}

/// One way to run a command.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shortcut {
    /// As shown to the user: a GPUI keystroke, or the prefix and the key
    /// typed after it, separated by a space.
    label: String,
    /// The key that completes a prefix chord. `None` binds `label` directly
    /// through GPUI's keymap.
    chord: Option<Keystroke>,
}

impl Shortcut {
    fn direct(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            chord: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    /// Parallel to `COMMANDS`, primary shortcut first.
    shortcuts: Vec<Vec<Shortcut>>,
    /// The keystroke that starts a chord, unless nothing can use it.
    prefix: Option<Keystroke>,
}

impl Default for Keymap {
    /// The catalog under Herdr's own default `[keys]`, as when neither config
    /// file names a keystroke.
    fn default() -> Self {
        Self::layer(
            vec![None; COMMANDS.len()],
            &Claimed::new(),
            &DaemonKeys::default(),
        )
    }
}

impl Keymap {
    /// Layers the daemon's `keys` and then `overrides` over the defaults. A
    /// keystroke the GUI config assigns moves to that command, so rebinding
    /// one key never requires unbinding its default or daemon owner too; two
    /// configured commands claiming it is an error. A command the GUI config
    /// names keeps exactly the keystrokes listed there.
    pub(crate) fn with_overrides(
        overrides: &BTreeMap<String, Binding>,
        keys: &DaemonKeys,
    ) -> Result<Self> {
        let mut configured = vec![None; COMMANDS.len()];
        for (name, binding) in overrides {
            let index = COMMANDS
                .iter()
                .position(|info| info.name == name)
                .ok_or_else(|| Error::UnknownKeybinding(name.clone()))?;
            let command = COMMANDS[index].name;
            let keystrokes: Vec<String> = binding.keystrokes().map(str::to_owned).collect();
            if keystrokes.len() > MAX_KEYSTROKES {
                return Err(Error::TooManyKeystrokes(command));
            }
            configured[index] = Some(keystrokes);
        }
        let mut claimed = Claimed::new();
        for (info, keystrokes) in COMMANDS.iter().zip(&configured) {
            for keystroke in keystrokes.iter().flatten() {
                let parsed = parse(info.name, keystroke)?;
                if let Some(first) = claimed.insert(identity(&parsed), info.name)
                    && first != info.name
                {
                    return Err(Error::DuplicateKeystroke {
                        keystroke: keystroke.clone(),
                        first,
                        second: info.name,
                    });
                }
            }
        }
        Ok(Self::layer(configured, &claimed, keys))
    }

    /// Herdr owns and validates its own file, so a daemon binding this client
    /// cannot honor, or that collides with one already placed, is skipped
    /// rather than reported: the first daemon binding for a keystroke wins,
    /// as does any GUI-configured keystroke over the daemon's.
    fn layer(configured: Vec<Option<Vec<String>>>, claimed: &Claimed, keys: &DaemonKeys) -> Self {
        // Keystrokes bound directly so far, which later layers cannot take.
        let mut taken: HashSet<_> = claimed.keys().cloned().collect();
        let prefix = (usable_prefix(&keys.prefix) && taken.insert(identity(&keys.prefix)))
            .then(|| keys.prefix.clone());
        let mut chords = HashSet::new();
        let mut from_daemon = vec![Vec::new(); COMMANDS.len()];
        for (command, trigger) in &keys.bindings {
            let Some(index) = COMMANDS.iter().position(|info| info.command == *command) else {
                continue;
            };
            if configured[index].is_some() {
                continue;
            }
            let shortcut = match trigger {
                Trigger::Direct(keystroke) => {
                    if !has_modifier(keystroke) || !taken.insert(identity(keystroke)) {
                        continue;
                    }
                    Shortcut::direct(keystroke.unparse())
                }
                Trigger::Prefixed(keystroke) => {
                    // The prefix typed twice sends it to the terminal instead.
                    let Some(prefix) = &prefix else {
                        continue;
                    };
                    if identity(keystroke) == identity(prefix)
                        || !chords.insert(identity(keystroke))
                    {
                        continue;
                    }
                    Shortcut {
                        label: format!("{} {}", prefix.unparse(), keystroke.unparse()),
                        chord: Some(keystroke.clone()),
                    }
                }
            };
            from_daemon[index].push(shortcut);
        }
        let shortcuts = COMMANDS
            .iter()
            .zip(configured)
            .zip(from_daemon)
            .map(|((info, configured), from_daemon)| match configured {
                Some(keystrokes) => keystrokes.into_iter().map(Shortcut::direct).collect(),
                None => info
                    .shortcuts
                    .iter()
                    .filter(|shortcut| {
                        Keystroke::parse(shortcut)
                            .is_ok_and(|parsed| !taken.contains(&identity(&parsed)))
                    })
                    .map(|&shortcut| Shortcut::direct(shortcut))
                    .chain(from_daemon)
                    .collect(),
            })
            .collect();
        Self { shortcuts, prefix }
    }

    /// Every shortcut bound to `command`, primary first. A prefix chord reads
    /// as the prefix and the key after it, separated by a space.
    pub fn shortcuts(&self, command: Command) -> impl Iterator<Item = &str> {
        COMMANDS
            .iter()
            .position(|info| info.command == command)
            .and_then(|index| self.shortcuts.get(index))
            .into_iter()
            .flatten()
            .map(|shortcut| shortcut.label.as_str())
    }

    /// The shortcut shown beside `command`, or `""` when it is unbound.
    pub fn primary(&self, command: Command) -> &str {
        self.shortcuts(command).next().unwrap_or("")
    }

    /// Each keystroke GPUI binds directly, with the command it runs, in
    /// catalog order. Prefix chords are not among them; see `chord`.
    pub fn bindings(&self) -> impl Iterator<Item = (Command, &str)> {
        COMMANDS
            .iter()
            .zip(&self.shortcuts)
            .flat_map(|(info, shortcuts)| {
                shortcuts
                    .iter()
                    .filter(|shortcut| shortcut.chord.is_none())
                    .map(move |shortcut| (info.command, shortcut.label.as_str()))
            })
    }

    /// The prefix as shown to the user, while chords can use it.
    pub(crate) fn prefix_label(&self) -> Option<String> {
        self.prefix.as_ref().map(Keystroke::unparse)
    }

    /// Whether `typed` is the keystroke that starts a chord.
    pub(crate) fn is_prefix(&self, typed: &Keystroke) -> bool {
        self.prefix
            .as_ref()
            .is_some_and(|prefix| typed_matches(typed, prefix))
    }

    /// The command a chord runs when `typed` follows the prefix.
    pub(crate) fn chord(&self, typed: &Keystroke) -> Option<Command> {
        COMMANDS
            .iter()
            .zip(&self.shortcuts)
            .find(|(_, shortcuts)| {
                shortcuts.iter().any(|shortcut| {
                    shortcut
                        .chord
                        .as_ref()
                        .is_some_and(|chord| typed_matches(typed, chord))
                })
            })
            .map(|(info, _)| info.command)
    }
}

/// GPUI's own matching, so a shifted symbol such as `?` matches however the
/// keyboard reports it.
fn typed_matches(typed: &Keystroke, bound: &Keystroke) -> bool {
    typed.should_match(&KeybindingKeystroke::from_keystroke(bound.clone()))
}

/// A keystroke without one of these would stop typing from reaching the
/// terminal.
fn has_modifier(keystroke: &Keystroke) -> bool {
    let modifiers = keystroke.modifiers;
    modifiers.platform || modifiers.control || modifiers.alt || modifiers.function
}

/// Herdr documents `esc` and function keys as prefixes; any other bare key
/// would swallow ordinary typing.
fn usable_prefix(prefix: &Keystroke) -> bool {
    has_modifier(prefix)
        || prefix.key == "escape"
        || prefix
            .key
            .strip_prefix('f')
            .is_some_and(|number| number.parse::<u8>().is_ok())
}

fn parse(command: &'static str, keystroke: &str) -> Result<Keystroke> {
    let parsed = Keystroke::parse(keystroke).map_err(|source| Error::InvalidKeystroke {
        command,
        keystroke: keystroke.to_owned(),
        source,
    })?;
    if !has_modifier(&parsed) {
        return Err(Error::KeystrokeWithoutModifier {
            command,
            keystroke: keystroke.to_owned(),
        });
    }
    Ok(parsed)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn overrides(entries: &[(&str, Binding)]) -> BTreeMap<String, Binding> {
        entries
            .iter()
            .map(|(name, binding)| ((*name).to_owned(), binding.clone()))
            .collect()
    }

    fn one(keystroke: &str) -> Binding {
        Binding::One(keystroke.into())
    }

    fn keystroke(text: &str) -> Keystroke {
        Keystroke::parse(text).unwrap()
    }

    /// A daemon config that binds nothing, isolating the GUI layers.
    fn no_keys() -> DaemonKeys {
        DaemonKeys {
            prefix: keystroke("ctrl-b"),
            bindings: Vec::new(),
        }
    }

    fn with(entries: &[(&str, Binding)]) -> Result<Keymap> {
        Keymap::with_overrides(&overrides(entries), &no_keys())
    }

    fn list(keymap: &Keymap, command: Command) -> Vec<&str> {
        keymap.shortcuts(command).collect()
    }

    #[test]
    fn defaults_follow_the_catalog_and_herdr() {
        let keymap = Keymap::default();
        assert_eq!(list(&keymap, Command::Tab), ["cmd-t", "ctrl-b c"]);
        assert_eq!(keymap.primary(Command::Tab), "cmd-t");
        assert_eq!(keymap.primary(Command::Workspace), "cmd-shift-n");
        assert_eq!(keymap.primary(Command::Themes), "");
        assert_eq!(
            list(&keymap, Command::SplitDown),
            ["cmd-shift-d", "ctrl-b -"]
        );
        assert_eq!(
            Keymap::with_overrides(&BTreeMap::new(), &DaemonKeys::default()).unwrap(),
            Keymap::default()
        );
        // Herdr's defaults are all chords, so GPUI binds only the catalog.
        let bound: usize = COMMANDS.iter().map(|info| info.shortcuts.len()).sum();
        assert_eq!(keymap.bindings().count(), bound);
        assert!(
            keymap
                .bindings()
                .all(|(_, keystroke)| !keystroke.contains(' '))
        );
    }

    #[test]
    fn overrides_replace_lists_and_empty_values_unbind() {
        let keymap = with(&[
            ("new_workspace", one("cmd-alt-t")),
            (
                "themes",
                Binding::Many(vec!["ctrl-shift-t".into(), " cmd-k ".into()]),
            ),
            ("close_pane", one("")),
            ("toggle_sidebar", Binding::Many(Vec::new())),
        ])
        .unwrap();
        assert_eq!(list(&keymap, Command::Workspace), ["cmd-alt-t"]);
        assert_eq!(list(&keymap, Command::Themes), ["ctrl-shift-t", "cmd-k"]);
        assert!(list(&keymap, Command::ClosePane).is_empty());
        assert!(list(&keymap, Command::ToggleSidebar).is_empty());
        assert_eq!(list(&keymap, Command::Tab), ["cmd-t"]);
    }

    /// Taking a default keystroke must not force the user to also unbind it
    /// from the command that shipped with it.
    #[test]
    fn configured_keystroke_moves_from_its_default_owner() {
        let keymap = with(&[("new_workspace", one("cmd-t"))]).unwrap();
        assert_eq!(list(&keymap, Command::Workspace), ["cmd-t"]);
        assert!(list(&keymap, Command::Tab).is_empty());
        // Spelling differences still name the same keystroke.
        let keymap = with(&[("about", one("CMD-shift-P"))]).unwrap();
        assert!(list(&keymap, Command::Palette).is_empty());
        let keys: Vec<_> = keymap.bindings().map(|(_, keystroke)| keystroke).collect();
        let unique: HashSet<_> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len());
    }

    #[test]
    fn invalid_configuration_reports_the_command() {
        let error = |entries: &[(&str, Binding)]| with(entries).unwrap_err();
        assert!(matches!(
            error(&[("new_space", one("cmd-n"))]),
            Error::UnknownKeybinding(name) if name == "new_space"
        ));
        let invalid = error(&[("new_tab", one("cmd-n-t"))]);
        assert!(std::error::Error::source(&invalid).is_some());
        assert!(matches!(
            invalid,
            Error::InvalidKeystroke { command: "new_tab", ref keystroke, .. } if keystroke == "cmd-n-t"
        ));
        for keystroke in ["n", "shift-n"] {
            assert!(matches!(
                error(&[("new_tab", one(keystroke))]),
                Error::KeystrokeWithoutModifier {
                    command: "new_tab",
                    ..
                }
            ));
        }
        assert!(matches!(
            error(&[("new_tab", Binding::Many(vec!["cmd-n".into(); 9]))]),
            Error::TooManyKeystrokes("new_tab")
        ));
        assert!(matches!(
            error(&[("new_tab", one("cmd-k")), ("themes", one("cmd-k"))]),
            Error::DuplicateKeystroke {
                first: "new_tab",
                second: "themes",
                ..
            }
        ));
        // Repeating a keystroke within one command is harmless.
        with(&[(
            "new_tab",
            Binding::Many(vec!["cmd-k".into(), "cmd-k".into()]),
        )])
        .unwrap();
    }

    #[test]
    fn chords_follow_the_prefix() {
        let keymap = Keymap::default();
        assert!(keymap.is_prefix(&keystroke("ctrl-b")));
        assert!(!keymap.is_prefix(&keystroke("ctrl-a")));
        assert!(!keymap.is_prefix(&keystroke("b")));
        assert_eq!(keymap.chord(&keystroke("c")), Some(Command::Tab));
        assert_eq!(keymap.chord(&keystroke("v")), Some(Command::SplitRight));
        assert_eq!(
            keymap.chord(&keystroke("shift-n")),
            Some(Command::Workspace)
        );
        assert_eq!(keymap.chord(&keystroke("3")), Some(Command::TabNumber(3)));
        assert_eq!(
            keymap.chord(&keystroke("shift-tab")),
            Some(Command::PreviousPane)
        );
        // A shifted symbol matches however the keyboard reports it.
        assert_eq!(
            keymap.chord(&keystroke("shift-/->?")),
            Some(Command::Keybinds)
        );
        assert_eq!(keymap.chord(&keystroke("?")), Some(Command::Keybinds));
        assert_eq!(keymap.chord(&keystroke("n")), Some(Command::NextTab));
        assert_eq!(keymap.chord(&keystroke("y")), None);
        assert_eq!(keymap.chord(&keystroke("ctrl-c")), None);
    }

    #[test]
    fn gui_overrides_replace_daemon_bindings() {
        let keys = DaemonKeys {
            prefix: keystroke("ctrl-a"),
            bindings: vec![
                (Command::Tab, Trigger::Prefixed(keystroke("c"))),
                (Command::Tab, Trigger::Direct(keystroke("alt-t"))),
                (Command::SplitRight, Trigger::Prefixed(keystroke("v"))),
            ],
        };
        let keymap =
            Keymap::with_overrides(&overrides(&[("new_tab", one("cmd-y"))]), &keys).unwrap();
        assert_eq!(list(&keymap, Command::Tab), ["cmd-y"]);
        assert_eq!(keymap.chord(&keystroke("c")), None);
        assert_eq!(list(&keymap, Command::SplitRight), ["cmd-d", "ctrl-a v"]);
        assert_eq!(keymap.chord(&keystroke("v")), Some(Command::SplitRight));
        // An empty GUI entry unbinds the daemon's chords too.
        let keymap =
            Keymap::with_overrides(&overrides(&[("split_right", one(""))]), &keys).unwrap();
        assert!(list(&keymap, Command::SplitRight).is_empty());
        assert_eq!(keymap.chord(&keystroke("v")), None);
    }

    #[test]
    fn daemon_keystrokes_move_from_gui_defaults_but_not_from_gui_config() {
        let keys = DaemonKeys {
            prefix: keystroke("ctrl-a"),
            bindings: vec![
                // cmd-d is Split Right's catalog default.
                (Command::Zoom, Trigger::Direct(keystroke("cmd-d"))),
                // The first daemon binding for a keystroke wins.
                (Command::Tab, Trigger::Direct(keystroke("alt-1"))),
                (Command::TabNumber(1), Trigger::Direct(keystroke("alt-1"))),
                (Command::Tab, Trigger::Prefixed(keystroke("c"))),
                (Command::CloseTab, Trigger::Prefixed(keystroke("c"))),
                // Configured below, so the GUI keeps it.
                (Command::ClosePane, Trigger::Direct(keystroke("cmd-e"))),
                // Would swallow typing.
                (Command::NextTab, Trigger::Direct(keystroke("shift-n"))),
                // The prefix typed twice passes it through instead.
                (Command::PreviousTab, Trigger::Prefixed(keystroke("ctrl-a"))),
            ],
        };
        let keymap = Keymap::with_overrides(&overrides(&[("about", one("cmd-e"))]), &keys).unwrap();
        // Daemon keystrokes read in GPUI's platform spelling (`super-d` on Linux).
        let moved = keystroke("cmd-d").unparse();
        assert_eq!(
            list(&keymap, Command::Zoom),
            ["cmd-shift-enter", moved.as_str()]
        );
        assert!(list(&keymap, Command::SplitRight).is_empty());
        assert_eq!(list(&keymap, Command::Tab), ["cmd-t", "alt-1", "ctrl-a c"]);
        assert_eq!(list(&keymap, Command::TabNumber(1)), ["cmd-1"]);
        assert_eq!(list(&keymap, Command::CloseTab), ["cmd-shift-w"]);
        assert_eq!(list(&keymap, Command::ClosePane), ["cmd-w"]);
        assert_eq!(list(&keymap, Command::About), ["cmd-e"]);
        assert_eq!(list(&keymap, Command::NextTab), ["cmd-shift-]"]);
        assert_eq!(list(&keymap, Command::PreviousTab), ["cmd-shift-["]);
        let keys: Vec<_> = keymap.bindings().map(|(_, keystroke)| keystroke).collect();
        let unique: HashSet<_> = keys.iter().collect();
        assert_eq!(keys.len(), unique.len());
    }

    #[test]
    fn the_prefix_yields_to_gui_config_and_typing() {
        let keys = |prefix| DaemonKeys {
            prefix: keystroke(prefix),
            bindings: vec![(Command::Tab, Trigger::Prefixed(keystroke("c")))],
        };
        // cmd-b is Toggle Sidebar's catalog default; the prefix takes it.
        let keymap = Keymap::with_overrides(&BTreeMap::new(), &keys("cmd-b")).unwrap();
        assert!(list(&keymap, Command::ToggleSidebar).is_empty());
        assert!(keymap.is_prefix(&keystroke("cmd-b")));
        // A GUI-configured keystroke keeps its command and disables chords.
        let keymap =
            Keymap::with_overrides(&overrides(&[("themes", one("ctrl-b"))]), &keys("ctrl-b"))
                .unwrap();
        assert!(!keymap.is_prefix(&keystroke("ctrl-b")));
        assert_eq!(keymap.chord(&keystroke("c")), None);
        assert_eq!(list(&keymap, Command::Tab), ["cmd-t"]);
        for (prefix, usable) in [
            ("f12", true),
            ("escape", true),
            ("a", false),
            ("shift-a", false),
        ] {
            let keymap = Keymap::with_overrides(&BTreeMap::new(), &keys(prefix)).unwrap();
            assert_eq!(keymap.is_prefix(&keystroke(prefix)), usable, "{prefix}");
            assert_eq!(keymap.chord(&keystroke("c")).is_some(), usable, "{prefix}");
        }
    }
}
