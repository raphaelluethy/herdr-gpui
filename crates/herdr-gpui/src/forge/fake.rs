//! A fake forge CLI for tests: a shell script standing in for `gh` or `glab`
//! at an explicit path, so no test ever runs the user's own CLI. It records
//! each argument on its own line in a log beside it.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

pub(crate) struct Fake {
    root: tempfile::TempDir,
    pub(crate) program: PathBuf,
}

impl Fake {
    /// `body` is shell run after the arguments are logged.
    pub(crate) fn new(name: &str, body: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join(name);
        let log = root.path().join("log");
        let script = format!(
            "#!/bin/sh\nLOG='{}'\nfor argument in \"$@\"; do printf '%s\\n' \"$argument\" >> \"$LOG\"; done\n{body}\n",
            log.display()
        );
        // Written by a separate process, so a concurrent test's fork cannot
        // hold the script open for writing when it runs (Linux ETXTBSY).
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

    /// Every argument the fake was run with, in order, across all runs.
    pub(crate) fn log(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
