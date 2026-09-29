//! Content polling also handles editors that replace the file by rename. All
//! reads happen on the background executor; two equal samples debounce saves.
use std::{
    collections::hash_map::DefaultHasher,
    fs::File,
    hash::Hasher,
    io::{self, Read},
    path::Path,
};

type Fingerprint = Result<u64, io::ErrorKind>;

pub(crate) fn fingerprint(path: &Path) -> Fingerprint {
    let read = || -> io::Result<u64> {
        let mut file = File::open(path)?;
        let mut hash = DefaultHasher::new();
        let mut buffer = [0; 8192];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                return Ok(hash.finish());
            }
            hash.write(&buffer[..count]);
        }
    };
    read().map_err(|error| error.kind())
}

/// Debounces samples of a file, or of several read together.
pub(crate) struct Watch<T = Fingerprint> {
    observed: Option<T>,
    accepted: Option<T>,
}

impl<T> Default for Watch<T> {
    fn default() -> Self {
        Self {
            observed: None,
            accepted: None,
        }
    }
}

impl<T: Copy + PartialEq> Watch<T> {
    pub(crate) fn observe(&mut self, current: T) -> bool {
        let stable = self.observed == Some(current);
        self.observed = Some(current);
        stable && self.accepted != Some(current)
    }

    /// Acknowledge the sample whose load completed, not a newer edit observed
    /// in the meantime. Cancelled loads must leave their sample pending.
    pub(crate) fn accept(&mut self, sample: T) {
        self.accepted = Some(sample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounces_and_retains_changes_until_accepted() {
        let mut watch: Watch = Watch::default();
        assert!(!watch.observe(Ok(1)));
        assert!(watch.observe(Ok(1)));
        watch.accept(Ok(1));
        assert!(!watch.observe(Ok(1)));
        assert!(!watch.observe(Ok(2)));
        assert!(!watch.observe(Ok(3)));
        assert!(watch.observe(Ok(3)));
        assert!(watch.observe(Ok(3)), "busy UI must not lose the change");
        watch.accept(Ok(3));
        assert!(!watch.observe(Ok(3)));
        assert!(!watch.observe(Err(io::ErrorKind::NotFound)));
        assert!(watch.observe(Err(io::ErrorKind::NotFound)));
        watch.accept(Err(io::ErrorKind::NotFound));
        assert!(!watch.observe(Err(io::ErrorKind::NotFound)));
        assert!(!watch.observe(Ok(3)));
        assert!(watch.observe(Ok(3)), "recreation must reload");
    }

    #[test]
    fn a_completed_load_does_not_acknowledge_a_newer_edit() {
        let mut watch: Watch = Watch::default();
        assert!(!watch.observe(Ok(1)));
        assert!(watch.observe(Ok(1)));
        assert!(!watch.observe(Ok(2)));
        watch.accept(Ok(1));
        assert!(watch.observe(Ok(2)));
        watch.accept(Ok(2));
        assert!(!watch.observe(Ok(2)));
    }

    #[test]
    fn detects_same_length_edits_atomic_replacement_and_recreation() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[terminal]\nsize=14")?;
        let original = fingerprint(&path);
        assert!(original.is_ok());
        std::fs::write(&path, "[terminal]\nsize=18")?;
        assert_ne!(original, fingerprint(&path));
        let mut replacement = tempfile::NamedTempFile::new_in(directory.path())?;
        use std::io::Write;
        replacement.write_all(b"[terminal]\nsize=20")?;
        let before = fingerprint(&path);
        replacement.persist(&path)?;
        assert_ne!(before, fingerprint(&path));
        std::fs::remove_file(&path)?;
        assert_eq!(fingerprint(&path), Err(io::ErrorKind::NotFound));
        std::fs::write(&path, "[terminal]\nsize=14")?;
        assert_eq!(original, fingerprint(&path));
        Ok(())
    }
}
