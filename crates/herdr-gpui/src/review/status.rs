//! The changed files of a checkout, read from `git status --porcelain=v1 -z`
//! and classified the way Zed's git panel does: conflicts first, then tracked
//! changes, then untracked files. Paths are Git's, so they are untrusted until
//! they are checked to stay inside the checkout.
use std::path::{Component, Path};

/// Which list of the panel a file belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Section {
    Conflicts,
    Tracked,
    Untracked,
}

impl Section {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Tracked => "Tracked",
            Self::Untracked => "Untracked",
        }
    }
}

/// What happened to a file, relative to the base it is compared with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Conflict,
    Untracked,
    Added,
    Deleted,
    Modified,
    Renamed,
    TypeChanged,
}

impl Kind {
    /// The one-letter status Zed and Git both print in the row.
    pub(crate) fn letter(self) -> &'static str {
        match self {
            Self::Conflict => "!",
            Self::Untracked => "?",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Modified => "M",
            Self::Renamed => "R",
            Self::TypeChanged => "T",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Conflict => "Conflict",
            Self::Untracked => "Untracked",
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Modified => "Modified",
            Self::Renamed => "Renamed",
            Self::TypeChanged => "Type changed",
        }
    }
}

/// How much of a file's change the index holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Staged {
    None,
    Partial,
    All,
}

/// One changed file: its repository-relative path and Git's two status
/// letters, index then working tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub path: String,
    pub index: u8,
    pub worktree: u8,
}

impl Entry {
    pub(crate) fn new(path: &str, index: u8, worktree: u8) -> Self {
        Self {
            path: path.to_owned(),
            index,
            worktree,
        }
    }

    pub(crate) fn kind(&self) -> Kind {
        let (index, worktree) = (self.index, self.worktree);
        if index == b'?' && worktree == b'?' {
            return Kind::Untracked;
        }
        let unmerged = (index == worktree && matches!(index, b'A' | b'D'))
            || index == b'U'
            || worktree == b'U';
        if unmerged {
            return Kind::Conflict;
        }
        let either = |letter: u8| index == letter || worktree == letter;
        if either(b'D') {
            Kind::Deleted
        } else if either(b'A') || either(b'C') {
            Kind::Added
        } else if either(b'R') {
            Kind::Renamed
        } else if either(b'T') {
            Kind::TypeChanged
        } else {
            Kind::Modified
        }
    }

    pub(crate) fn section(&self) -> Section {
        match self.kind() {
            Kind::Conflict => Section::Conflicts,
            Kind::Untracked => Section::Untracked,
            _ => Section::Tracked,
        }
    }

    /// A conflict is never staged: it needs resolving first, and Git marks
    /// both sides.
    pub(crate) fn staged(&self) -> Staged {
        match self.kind() {
            Kind::Untracked | Kind::Conflict => Staged::None,
            _ => match (self.index, self.worktree) {
                (b' ', _) => Staged::None,
                (_, b' ') => Staged::All,
                _ => Staged::Partial,
            },
        }
    }

    pub(crate) fn is_deleted(&self) -> bool {
        self.kind() == Kind::Deleted
    }

    pub(crate) fn is_untracked(&self) -> bool {
        self.kind() == Kind::Untracked
    }

    /// The file name and, when it has one, the folder it is in.
    pub(crate) fn name_and_dir(&self) -> (&str, &str) {
        match self.path.rsplit_once('/') {
            Some((dir, name)) => (name, dir),
            None => (self.path.as_str(), ""),
        }
    }
}

/// Whether a path Git printed may be joined to the checkout: relative, with
/// no `..`, no drive prefix, and no NUL. Everything the panel opens, stages, or
/// diffs goes through here first.
pub(crate) fn safe_relative(path: &str) -> bool {
    if path.is_empty() || path.contains('\0') {
        return false;
    }
    let path = Path::new(path);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Parses `git status --porcelain=v1 --untracked-files=all --no-renames -z`.
/// Records are `XY path` separated by NUL; `--no-renames` keeps every record
/// to one path. Ignored entries and unsafe paths are dropped.
pub(crate) fn parse_porcelain(output: &str) -> Vec<Entry> {
    output
        .split('\0')
        .filter_map(|record| {
            let bytes = record.as_bytes();
            if bytes.len() < 4 || bytes[2] != b' ' {
                return None;
            }
            let (index, worktree) = (bytes[0], bytes[1]);
            if index == b'!' && worktree == b'!' {
                return None;
            }
            let path = &record[3..];
            safe_relative(path).then(|| Entry::new(path, index, worktree))
        })
        .collect()
}

/// Parses `git diff --name-status --no-renames -z <base>`: records of a status
/// letter and a path, both NUL-terminated. The letter goes in the working tree
/// column; nothing in this listing is staged, since it compares with a commit.
pub(crate) fn parse_name_status(output: &str) -> Vec<Entry> {
    let mut fields = output.split('\0');
    let mut entries = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let Some(&letter) = status.as_bytes().first() else {
            continue;
        };
        if safe_relative(path) {
            entries.push(Entry::new(path, b' ', letter));
        }
    }
    entries
}

/// Untracked paths from `git ls-files --others --exclude-standard -z`, as
/// entries of the untracked section.
pub(crate) fn parse_untracked(output: &str) -> Vec<Entry> {
    output
        .split('\0')
        .filter(|path| safe_relative(path))
        .map(|path| Entry::new(path, b'?', b'?'))
        .collect()
}

/// Sums `git diff --numstat` output; binary rows count no lines.
pub(crate) fn sum_numstat(output: &str) -> (u64, u64) {
    output.lines().fold((0, 0), |(added, removed), line| {
        let mut fields = line.split('\t');
        let parse = |field: Option<&str>| field.and_then(|field| field.parse::<u64>().ok());
        let additions = parse(fields.next()).unwrap_or(0);
        let deletions = parse(fields.next()).unwrap_or(0);
        (
            added.saturating_add(additions),
            removed.saturating_add(deletions),
        )
    })
}

/// Orders entries by section, then by path, so the list is stable between
/// refreshes however Git ordered them.
pub(crate) fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        a.section()
            .cmp(&b.section())
            .then_with(|| a.path.cmp(&b.path))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(output: &str) -> Vec<(String, Kind, Staged)> {
        parse_porcelain(output)
            .into_iter()
            .map(|entry| {
                let kind = entry.kind();
                let staged = entry.staged();
                (entry.path, kind, staged)
            })
            .collect()
    }

    #[test]
    fn porcelain_records_are_classified_like_zeds_git_panel() {
        let output = concat!(
            "?? new.txt\0",
            "!! build/\0",
            "M  staged.rs\0",
            " M unstaged.rs\0",
            "MM partial.rs\0",
            "A  added.rs\0",
            "AM added-then-edited.rs\0",
            " D deleted.rs\0",
            "D  staged-deletion.rs\0",
            "MD edited-then-deleted.rs\0",
            "UU both.rs\0",
            "AA both-added.rs\0",
            "DD both-deleted.rs\0",
            "AU added-by-us.rs\0",
            "UD deleted-by-them.rs\0",
            " T typechange\0",
            "R  renamed.rs\0",
        );
        assert_eq!(
            kinds(output),
            [
                ("new.txt", Kind::Untracked, Staged::None),
                ("staged.rs", Kind::Modified, Staged::All),
                ("unstaged.rs", Kind::Modified, Staged::None),
                ("partial.rs", Kind::Modified, Staged::Partial),
                ("added.rs", Kind::Added, Staged::All),
                ("added-then-edited.rs", Kind::Added, Staged::Partial),
                ("deleted.rs", Kind::Deleted, Staged::None),
                ("staged-deletion.rs", Kind::Deleted, Staged::All),
                ("edited-then-deleted.rs", Kind::Deleted, Staged::Partial),
                ("both.rs", Kind::Conflict, Staged::None),
                ("both-added.rs", Kind::Conflict, Staged::None),
                ("both-deleted.rs", Kind::Conflict, Staged::None),
                ("added-by-us.rs", Kind::Conflict, Staged::None),
                ("deleted-by-them.rs", Kind::Conflict, Staged::None),
                ("typechange", Kind::TypeChanged, Staged::None),
                ("renamed.rs", Kind::Renamed, Staged::All),
            ]
            .map(|(path, kind, staged)| (path.to_owned(), kind, staged))
        );
        let sections: Vec<_> = parse_porcelain(output).iter().map(Entry::section).collect();
        assert_eq!(sections[0], Section::Untracked);
        assert_eq!(sections[1], Section::Tracked);
        assert_eq!(sections[9], Section::Conflicts);
    }

    #[test]
    fn nul_separated_paths_keep_spaces_unicode_and_newlines() {
        let output = "?? with space.txt\0 M caf\u{e9}/\u{4e2d}\u{6587}.rs\0?? line\nbreak.txt\0";
        let paths: Vec<_> = parse_porcelain(output)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(
            paths,
            [
                "with space.txt",
                "caf\u{e9}/\u{4e2d}\u{6587}.rs",
                "line\nbreak.txt"
            ]
        );
        assert_eq!(
            Entry::new("caf\u{e9}/\u{4e2d}\u{6587}.rs", b' ', b'M').name_and_dir(),
            ("\u{4e2d}\u{6587}.rs", "caf\u{e9}")
        );
        assert_eq!(Entry::new("top", b' ', b'M').name_and_dir(), ("top", ""));
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("M\0").is_empty(), "a truncated record");
    }

    #[test]
    fn paths_that_leave_the_checkout_are_refused() {
        for path in [
            "src/main.rs",
            "with space",
            "a/b/../c",
            "trailing/",
            "-starts-with-dash",
            ":pathspec",
        ] {
            let plain = !path
                .split('/')
                .any(|component| component == ".." || component == ".");
            assert_eq!(safe_relative(path), plain, "{path}");
        }
        for path in [
            "",
            "/etc/passwd",
            "../outside",
            "a/../../outside",
            "nul\0byte",
            "./here",
        ] {
            assert!(!safe_relative(path), "{path}");
        }
        #[cfg(windows)]
        {
            assert!(!safe_relative("C:\\outside"));
            assert!(!safe_relative("C:relative"));
            assert!(!safe_relative("\\\\server\\share"));
        }
        // Git never prints these, but a malicious path must still drop out.
        assert!(parse_porcelain("?? ../escape\0?? ok\0").len() == 1);
        assert!(parse_name_status("M\0/abs\0A\0fine\0").len() == 1);
        assert_eq!(parse_untracked("../x\0y\0\0").len(), 1);
    }

    #[test]
    fn name_status_and_untracked_listings_become_entries() {
        let entries = parse_name_status("M\0src/a.rs\0A\0new.rs\0D\0gone.rs\0T\0link\0");
        let kinds: Vec<_> = entries.iter().map(Entry::kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Modified,
                Kind::Added,
                Kind::Deleted,
                Kind::TypeChanged
            ]
        );
        assert!(entries.iter().all(|entry| entry.staged() == Staged::None));
        let untracked = parse_untracked("a\0b/c\0");
        assert!(untracked.iter().all(Entry::is_untracked));
        assert_eq!(untracked[1].path, "b/c");
    }

    #[test]
    fn numstat_sums_text_rows_and_entries_sort_by_section() {
        assert_eq!(
            sum_numstat("12\t3\tsrc/main.rs\n-\t-\tlogo.png\n0\t7\tREADME.md\n"),
            (12, 10)
        );
        assert_eq!(sum_numstat(""), (0, 0));
        let mut entries = parse_porcelain("?? z\0 M b\0UU a\0 M a\0");
        sort_entries(&mut entries);
        let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, ["a", "a", "b", "z"]);
        assert_eq!(entries[0].section(), Section::Conflicts);
        assert_eq!(entries[3].section(), Section::Untracked);
        assert_eq!(Kind::Deleted.letter(), "D");
        assert_eq!(Section::Untracked.label(), "Untracked");
    }
}
