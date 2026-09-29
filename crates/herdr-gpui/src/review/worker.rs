//! The Git behind the review panel: listing changed files, reading one file's
//! diff, and staging. Every command runs on the panel's worker thread through
//! the shared bounded process policy, against a checkout `local_checkout`
//! verified first, so a renamed branch or moved worktree is never worked on by
//! mistake. Staging is an explicit user action: it is never retried or
//! replayed, and, like every write, never cancelled halfway.
use super::{
    Mode,
    diff::{self, FileDiff, MAX_FILE_BYTES},
    status::{self, Entry},
};
use crate::{
    Error,
    git::git,
    pull_request::{Input, local_checkout},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Staging touches the index and may wait on another Git process's lock.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Git's empty tree: what an unborn branch is compared with.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// Where a branch diff's base is looked for when origin has no default branch.
const BASE_CANDIDATES: [&str; 4] = ["origin/main", "origin/master", "main", "master"];
/// Paths per `git add`, well under any platform's argument limit.
const PATHSPEC_CHUNK: usize = 128;
/// Files a listing shows before it is cut short.
pub(crate) const MAX_ENTRIES: usize = 2000;

/// What a branch diff compares the working tree with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Base {
    /// The ref, such as `origin/main`.
    pub name: String,
    pub merge_base: String,
}

/// The changed files of a checkout in one mode, and what they are compared
/// with. `root` is the checkout Git verified, the only path files are opened
/// under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Listing {
    pub mode: Mode,
    pub root: PathBuf,
    /// `None` on an unborn branch, which has nothing but the empty tree behind it.
    pub head: Option<String>,
    pub base: Option<Base>,
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub additions: u64,
    pub deletions: u64,
}

impl Listing {
    /// The revision a file's diff is taken against in this listing.
    pub(crate) fn compare(&self, entry: &Entry) -> Compare {
        if entry.is_untracked() {
            return Compare::Untracked;
        }
        let rev = match (&self.base, &self.head) {
            (Some(base), _) => base.merge_base.clone(),
            (None, Some(head)) => head.clone(),
            (None, None) => EMPTY_TREE.to_owned(),
        };
        Compare::Rev(rev)
    }

    pub(crate) fn unborn(&self) -> bool {
        self.head.is_none()
    }

    pub(crate) fn entry(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.path == path)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Compare {
    /// A file Git does not know: read it and show every line as added.
    Untracked,
    /// `git diff <rev> -- path`.
    Rev(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    List(Mode),
    Diff {
        mode: Mode,
        path: String,
        compare: Compare,
    },
    Stage(Vec<String>),
    Unstage {
        paths: Vec<String>,
        unborn: bool,
    },
    StageAll,
    UnstageAll {
        unborn: bool,
    },
}

impl Job {
    pub(crate) fn is_write(&self) -> bool {
        matches!(
            self,
            Self::Stage(_) | Self::Unstage { .. } | Self::StageAll | Self::UnstageAll { .. }
        )
    }
}

pub(crate) enum Completion {
    List(crate::Result<Listing>),
    Diff {
        mode: Mode,
        path: String,
        result: crate::Result<FileDiff>,
    },
    Write(crate::Result<()>),
}

pub(crate) fn execute(input: &Input, job: Job, cancelled: &impl Fn() -> bool) -> Completion {
    match job {
        Job::List(mode) => {
            Completion::List(list(input, mode, Instant::now() + READ_TIMEOUT, cancelled))
        }
        Job::Diff {
            mode,
            path,
            compare,
        } => {
            let result = file_diff(
                input,
                &path,
                &compare,
                Instant::now() + READ_TIMEOUT,
                cancelled,
            );
            Completion::Diff { mode, path, result }
        }
        // Writes are never cancelled: killing `git add` halfway can leave an
        // index lock behind, so only their own deadline ends them.
        job => Completion::Write(write(input, job, Instant::now() + WRITE_TIMEOUT)),
    }
}

/// A pathspec that names exactly `path`: no globbing, and a leading `:` or
/// `-` cannot turn into magic or an option.
fn pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

/// The commit HEAD names, or `None` on an unborn branch.
fn head(
    checkout: &str,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<Option<String>> {
    match git(
        checkout,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        "read HEAD",
        deadline,
        cancelled,
    ) {
        Ok(head) if !head.is_empty() => Ok(Some(head)),
        Ok(_) | Err(Error::GitFailed { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The branch a branch diff compares with: origin's default branch when it is
/// known, otherwise the first usual name that exists.
fn resolve_base(
    checkout: &str,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<String> {
    match git(
        checkout,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        "read origin's default branch",
        deadline,
        cancelled,
    ) {
        Ok(name) if !name.is_empty() => return Ok(name),
        Ok(_) | Err(Error::GitFailed { .. }) => {}
        Err(error) => return Err(error),
    }
    for candidate in BASE_CANDIDATES {
        match git(
            checkout,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{candidate}^{{commit}}"),
            ],
            "look for a base branch",
            deadline,
            cancelled,
        ) {
            Ok(sha) if !sha.is_empty() => return Ok(candidate.to_owned()),
            Ok(_) | Err(Error::GitFailed { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Err(Error::ReviewNoBase)
}

fn list(
    input: &Input,
    mode: Mode,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<Listing> {
    let checkout = local_checkout(input, deadline, cancelled)?;
    let head = head(&checkout, deadline, cancelled)?;
    let (base, mut entries, against) = match mode {
        Mode::Uncommitted => {
            let output = git(
                &checkout,
                &[
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--no-renames",
                    "-z",
                ],
                "read the working tree status",
                deadline,
                cancelled,
            )?;
            let against = head.clone().unwrap_or_else(|| EMPTY_TREE.to_owned());
            (None, status::parse_porcelain(&output), against)
        }
        Mode::Branch => {
            if head.is_none() {
                return Err(Error::ReviewUnborn);
            }
            let name = resolve_base(&checkout, deadline, cancelled)?;
            let merge_base = git(
                &checkout,
                &["merge-base", &name, "HEAD"],
                "find the merge base",
                deadline,
                cancelled,
            )?;
            if merge_base.is_empty() {
                return Err(Error::ReviewNoBase);
            }
            let changed = git(
                &checkout,
                &["diff", "--name-status", "--no-renames", "-z", &merge_base],
                "list changed files",
                deadline,
                cancelled,
            )?;
            let untracked = git(
                &checkout,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                "list untracked files",
                deadline,
                cancelled,
            )?;
            let mut entries = status::parse_name_status(&changed);
            entries.extend(status::parse_untracked(&untracked));
            (
                Some(Base {
                    name,
                    merge_base: merge_base.clone(),
                }),
                entries,
                merge_base,
            )
        }
    };
    let numstat = git(
        &checkout,
        &["diff", "--numstat", "--no-renames", &against],
        "count changed lines",
        deadline,
        cancelled,
    )?;
    let (additions, deletions) = status::sum_numstat(&numstat);
    status::sort_entries(&mut entries);
    let truncated = entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    Ok(Listing {
        mode,
        root: PathBuf::from(checkout),
        head,
        base,
        entries,
        truncated,
        additions,
        deletions,
    })
}

fn file_diff(
    input: &Input,
    path: &str,
    compare: &Compare,
    deadline: Instant,
    cancelled: &impl Fn() -> bool,
) -> crate::Result<FileDiff> {
    if !status::safe_relative(path) {
        return Err(Error::ReviewUnsafePath);
    }
    let checkout = local_checkout(input, deadline, cancelled)?;
    match compare {
        Compare::Untracked => read_added(&Path::new(&checkout).join(path)),
        Compare::Rev(rev) => {
            let output = git(
                &checkout,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--unified=3",
                    rev,
                    "--",
                    &pathspec(path),
                ],
                "read the file's diff",
                deadline,
                cancelled,
            )?;
            Ok(diff::parse_unified(&output))
        }
    }
}

/// An untracked file as an all-added diff, read up to the size bound. A
/// symbolic link shows as a change with no content, as Git lists it, rather
/// than following it out of the checkout.
fn read_added(path: &Path) -> crate::Result<FileDiff> {
    let file_error = |operation: &'static str| {
        move |source: std::io::Error| Error::ReviewFile { operation, source }
    };
    let metadata = std::fs::symlink_metadata(path).map_err(file_error("inspect"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(FileDiff {
            metadata_only: true,
            ..FileDiff::default()
        });
    }
    let mut content = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(0)
            .min(MAX_FILE_BYTES),
    );
    std::fs::File::open(path)
        .map_err(file_error("open"))?
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut content)
        .map_err(file_error("read"))?;
    let cut = content.len() > MAX_FILE_BYTES;
    content.truncate(MAX_FILE_BYTES);
    Ok(diff::added_file(&content, cut))
}

fn write(input: &Input, job: Job, deadline: Instant) -> crate::Result<()> {
    let never = || false;
    let checkout = local_checkout(input, deadline, &never)?;
    let each_chunk = |paths: Vec<String>, lead: &[&str], operation: &'static str| {
        if paths.iter().any(|path| !status::safe_relative(path)) {
            return Err(Error::ReviewUnsafePath);
        }
        for chunk in paths.chunks(PATHSPEC_CHUNK) {
            let specs: Vec<String> = chunk.iter().map(|path| pathspec(path)).collect();
            let mut args: Vec<&str> = lead.to_vec();
            args.push("--");
            args.extend(specs.iter().map(String::as_str));
            git(&checkout, &args, operation, deadline, &never)?;
        }
        Ok(())
    };
    match job {
        Job::Stage(paths) => each_chunk(paths, &["add"], "stage"),
        Job::Unstage { paths, unborn } if unborn => {
            each_chunk(paths, &["rm", "--cached", "-r", "--quiet"], "unstage")
        }
        Job::Unstage { paths, .. } => each_chunk(paths, &["restore", "--staged"], "unstage"),
        Job::StageAll => git(&checkout, &["add", "-A"], "stage all", deadline, &never).map(drop),
        Job::UnstageAll { unborn } if unborn => git(
            &checkout,
            &["rm", "--cached", "-r", "--quiet", "--", "."],
            "unstage all",
            deadline,
            &never,
        )
        .map(drop),
        Job::UnstageAll { .. } => git(
            &checkout,
            &["reset", "--quiet"],
            "unstage all",
            deadline,
            &never,
        )
        .map(drop),
        Job::List(_) | Job::Diff { .. } => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::review::{
        diff::Row,
        status::{Kind, Staged},
    };
    use std::process::Command;

    /// A repository on disk with one commit, its `Input`, and a runner for
    /// further Git commands against it.
    pub(crate) struct Repo {
        pub directory: tempfile::TempDir,
        _hooks: tempfile::TempDir,
        pub input: Input,
    }

    impl Repo {
        pub(crate) fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let hooks = tempfile::tempdir().unwrap();
            let checkout = directory.path().to_str().unwrap().to_owned();
            let input = Input {
                repo_key: directory.path().join(".git").to_str().unwrap().to_owned(),
                branch: "feature".into(),
                checkout: Some(checkout),
            };
            let repo = Self {
                directory,
                _hooks: hooks,
                input,
            };
            repo.git(&["init", "-b", "main"]);
            repo.write("tracked.txt", "one\ntwo\nthree\n");
            repo.write("keep.txt", "kept\n");
            repo.git(&["add", "-A"]);
            repo.git(&["commit", "-m", "initial"]);
            repo.git(&["checkout", "-q", "-b", "feature"]);
            repo
        }

        pub(crate) fn git(&self, args: &[&str]) -> String {
            let result = Command::new("git")
                .args([
                    "-C",
                    self.input.checkout.as_deref().unwrap(),
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                ])
                .arg(format!("core.hooksPath={}", self._hooks.path().display()))
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            String::from_utf8_lossy(&result.stdout).trim().to_owned()
        }

        pub(crate) fn write(&self, path: &str, content: &str) {
            let full = self.directory.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }

        pub(crate) fn run(&self, job: Job) -> Completion {
            execute(&self.input, job, &|| false)
        }

        pub(crate) fn list(&self, mode: Mode) -> Listing {
            match self.run(Job::List(mode)) {
                Completion::List(result) => result.unwrap(),
                _ => panic!("not a listing"),
            }
        }

        pub(crate) fn diff(&self, listing: &Listing, path: &str) -> FileDiff {
            let entry = listing.entry(path).unwrap();
            match self.run(Job::Diff {
                mode: listing.mode,
                path: path.into(),
                compare: listing.compare(entry),
            }) {
                Completion::Diff { result, .. } => result.unwrap(),
                _ => panic!("not a diff"),
            }
        }

        fn write_ok(&self, job: Job) {
            match self.run(job) {
                Completion::Write(result) => result.unwrap(),
                _ => panic!("not a write"),
            }
        }
    }

    #[test]
    fn uncommitted_listing_diffs_and_staging_follow_the_working_tree() {
        let repo = Repo::new();
        repo.write("tracked.txt", "one\n2\nthree\n");
        repo.write("dir with space/new file.txt", "fresh\n");
        std::fs::remove_file(repo.directory.path().join("keep.txt")).unwrap();
        let listing = repo.list(Mode::Uncommitted);
        assert_eq!(listing.mode, Mode::Uncommitted);
        assert!(listing.head.is_some() && listing.base.is_none());
        assert_eq!(
            listing.root.canonicalize().unwrap(),
            repo.directory.path().canonicalize().unwrap()
        );
        let paths: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind(), entry.staged()))
            .collect();
        assert_eq!(
            paths,
            [
                ("keep.txt", Kind::Deleted, Staged::None),
                ("tracked.txt", Kind::Modified, Staged::None),
                ("dir with space/new file.txt", Kind::Untracked, Staged::None),
            ]
        );
        assert_eq!((listing.additions, listing.deletions), (1, 2));

        let modified = repo.diff(&listing, "tracked.txt");
        assert_eq!(
            modified.rows,
            [
                Row::Hunk("@@ -1,3 +1,3 @@".into()),
                Row::Context {
                    old: 1,
                    new: 1,
                    text: "one".into()
                },
                Row::Removed {
                    old: 2,
                    text: "two".into()
                },
                Row::Added {
                    new: 2,
                    text: "2".into()
                },
                Row::Context {
                    old: 3,
                    new: 3,
                    text: "three".into()
                },
            ]
        );
        let deleted = repo.diff(&listing, "keep.txt");
        assert_eq!(deleted.deletions, 1);
        assert!(
            deleted
                .rows
                .iter()
                .any(|row| matches!(row, Row::Removed { text, .. } if text == "kept"))
        );
        let untracked = repo.diff(&listing, "dir with space/new file.txt");
        assert_eq!(
            untracked.rows[1],
            Row::Added {
                new: 1,
                text: "fresh".into()
            }
        );

        repo.write_ok(Job::Stage(vec!["tracked.txt".into()]));
        let listing = repo.list(Mode::Uncommitted);
        assert_eq!(listing.entry("tracked.txt").unwrap().staged(), Staged::All);
        repo.write("tracked.txt", "one\n2\n3\n");
        let listing = repo.list(Mode::Uncommitted);
        assert_eq!(
            listing.entry("tracked.txt").unwrap().staged(),
            Staged::Partial
        );
        repo.write_ok(Job::Unstage {
            paths: vec!["tracked.txt".into()],
            unborn: false,
        });
        assert_eq!(
            repo.list(Mode::Uncommitted)
                .entry("tracked.txt")
                .unwrap()
                .staged(),
            Staged::None
        );
        repo.write_ok(Job::StageAll);
        let listing = repo.list(Mode::Uncommitted);
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.staged() == Staged::All)
        );
        assert!(listing.entry("keep.txt").unwrap().is_deleted());
        repo.write_ok(Job::UnstageAll { unborn: false });
        let listing = repo.list(Mode::Uncommitted);
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.staged() == Staged::None)
        );
        // Untracked files are staged by path too, whatever they are named.
        repo.write("-dashed", "x\n");
        repo.write(":colon", "x\n");
        repo.write_ok(Job::Stage(vec!["-dashed".into(), ":colon".into()]));
        let listing = repo.list(Mode::Uncommitted);
        assert_eq!(listing.entry("-dashed").unwrap().kind(), Kind::Added);
        assert_eq!(listing.entry(":colon").unwrap().kind(), Kind::Added);
        // Nothing outside the checkout is ever named to Git or opened.
        assert!(matches!(
            repo.run(Job::Stage(vec!["../escape".into()])),
            Completion::Write(Err(Error::ReviewUnsafePath))
        ));
        assert!(matches!(
            repo.run(Job::Diff {
                mode: Mode::Uncommitted,
                path: "/etc/passwd".into(),
                compare: Compare::Untracked
            }),
            Completion::Diff {
                result: Err(Error::ReviewUnsafePath),
                ..
            }
        ));
    }

    #[test]
    fn a_branch_listing_compares_with_the_merge_base_of_the_default_branch() {
        let repo = Repo::new();
        // No origin: the local default branch is the base.
        repo.write("tracked.txt", "one\ntwo\nthree\nfour\n");
        repo.git(&["commit", "-qam", "on the branch"]);
        repo.write("tracked.txt", "one\ntwo\nthree\nfour\nfive\n");
        repo.write("untracked.txt", "u\n");
        let listing = repo.list(Mode::Branch);
        let base = listing.base.clone().unwrap();
        assert_eq!(base.name, "main");
        assert_eq!(base.merge_base, repo.git(&["rev-parse", "main"]));
        let paths: Vec<_> = listing
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.kind()))
            .collect();
        assert_eq!(
            paths,
            [
                ("tracked.txt", Kind::Modified),
                ("untracked.txt", Kind::Untracked)
            ]
        );
        assert_eq!((listing.additions, listing.deletions), (2, 0));
        let diff = repo.diff(&listing, "tracked.txt");
        assert_eq!(
            diff.additions, 2,
            "committed and uncommitted lines both show"
        );
        // Main moving on does not change the merge base.
        repo.git(&["commit", "-qam", "more on the branch"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.write("keep.txt", "kept\nmore\n");
        repo.git(&["commit", "-qam", "on main"]);
        repo.git(&["checkout", "-q", "feature"]);
        let listing = repo.list(Mode::Branch);
        assert_eq!(listing.base.as_ref().unwrap().merge_base, base.merge_base);
        assert!(listing.entry("keep.txt").is_none());
        // Origin's default branch wins once it is known.
        repo.git(&["update-ref", "refs/remotes/origin/main", "main"]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        assert_eq!(repo.list(Mode::Branch).base.unwrap().name, "origin/main");
    }

    #[test]
    fn an_unborn_branch_lists_against_the_empty_tree_and_has_no_base() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().to_str().unwrap().to_owned();
        let output = Command::new("git")
            .args(["-C", &checkout, "init", "-b", "fresh"])
            .output()
            .unwrap();
        assert!(output.status.success());
        std::fs::write(directory.path().join("a.txt"), "a\n").unwrap();
        let input = Input {
            repo_key: directory.path().join(".git").to_str().unwrap().to_owned(),
            branch: "fresh".into(),
            checkout: Some(checkout),
        };
        let run = |job| execute(&input, job, &|| false);
        let Completion::List(Ok(listing)) = run(Job::List(Mode::Uncommitted)) else {
            panic!("listing");
        };
        assert!(listing.unborn());
        assert_eq!(listing.entries[0].kind(), Kind::Untracked);
        assert!(matches!(
            run(Job::List(Mode::Branch)),
            Completion::List(Err(Error::ReviewUnborn))
        ));
        let Completion::Write(Ok(())) = run(Job::Stage(vec!["a.txt".into()])) else {
            panic!("stage");
        };
        let Completion::List(Ok(listing)) = run(Job::List(Mode::Uncommitted)) else {
            panic!("listing");
        };
        let entry = listing.entry("a.txt").unwrap();
        assert_eq!((entry.kind(), entry.staged()), (Kind::Added, Staged::All));
        assert_eq!(listing.compare(entry), Compare::Rev(EMPTY_TREE.into()));
        let Completion::Diff {
            result: Ok(diff), ..
        } = run(Job::Diff {
            mode: Mode::Uncommitted,
            path: "a.txt".into(),
            compare: listing.compare(entry),
        })
        else {
            panic!("diff");
        };
        assert_eq!(diff.additions, 1);
        let Completion::Write(Ok(())) = run(Job::Unstage {
            paths: vec!["a.txt".into()],
            unborn: true,
        }) else {
            panic!("unstage");
        };
        let Completion::List(Ok(listing)) = run(Job::List(Mode::Uncommitted)) else {
            panic!("listing");
        };
        assert_eq!(listing.entry("a.txt").unwrap().kind(), Kind::Untracked);
        let Completion::Write(Ok(())) = run(Job::StageAll) else {
            panic!("stage all");
        };
        let Completion::Write(Ok(())) = run(Job::UnstageAll { unborn: true }) else {
            panic!("unstage all");
        };
        let Completion::List(Ok(listing)) = run(Job::List(Mode::Uncommitted)) else {
            panic!("listing");
        };
        assert!(listing.entries.iter().all(Entry::is_untracked));
    }

    #[test]
    fn untracked_files_are_read_within_bounds_and_links_are_not_followed() {
        let directory = tempfile::tempdir().unwrap();
        let big = directory.path().join("big.bin");
        std::fs::write(&big, vec![b'x'; MAX_FILE_BYTES + 10]).unwrap();
        let diff = read_added(&big).unwrap();
        assert!(diff.truncated);
        assert_eq!(diff.rows.len(), 2, "one hunk header and one long line");
        assert!(matches!(
            read_added(&directory.path().join("missing")),
            Err(Error::ReviewFile {
                operation: "inspect",
                ..
            })
        ));
        assert!(read_added(directory.path()).unwrap().metadata_only);
        #[cfg(unix)]
        {
            let link = directory.path().join("link");
            std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
            let diff = read_added(&link).unwrap();
            assert!(diff.metadata_only && diff.rows.is_empty());
        }
        assert_eq!(pathspec("*.rs"), ":(literal)*.rs");
        assert!(Job::StageAll.is_write() && !Job::List(Mode::Branch).is_write());
    }
}
