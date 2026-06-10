//! Fault injection primitives.
//!
//! [`Fault`] is the extension point; [`TruncateFile`] is the built-in implementation.
//! Implement [`Fault`] for additional fault types (corrupt bytes, partial writes, etc.).

use std::fs;
use std::path::{Path, PathBuf};

use rand::Rng;
use rand::rngs::StdRng;

/// A single injectable fault.
pub trait Fault: Send {
    fn name(&self) -> &str;
    /// Inject the fault. `paths` is the slice of file paths the simulation manages
    /// (WAL file, disk dirs, etc.); the fault implementation may modify any of them.
    fn inject(&mut self, rng: &mut StdRng, paths: &[&Path]);
}

/// Truncate a specific file at a uniformly random byte offset.
pub struct TruncateFile {
    pub path: PathBuf,
}

impl Fault for TruncateFile {
    fn name(&self) -> &str {
        "truncate_file"
    }

    fn inject(&mut self, rng: &mut StdRng, _paths: &[&Path]) {
        truncate_file(&self.path, rng);
    }
}

/// Truncate any file at a uniformly random byte offset in `[0, len)`.
///
/// Returns bytes removed, or `None` if the file was empty or missing.
pub fn truncate_file(path: &Path, rng: &mut StdRng) -> Option<u64> {
    let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return None;
    }
    let keep = rng.gen_range(0..len);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|f| f.set_len(keep))
        .expect("file truncation failed");
    Some(len - keep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn truncate_file_reduces_size() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello world 1234567890").unwrap();
        f.flush().unwrap();
        let original_len = fs::metadata(f.path()).unwrap().len();
        let mut rng = StdRng::seed_from_u64(1);
        let removed = truncate_file(f.path(), &mut rng);
        assert!(removed.is_some());
        assert!(removed.unwrap() > 0);
        let new_len = fs::metadata(f.path()).unwrap().len();
        assert!(new_len < original_len);
    }

    #[test]
    fn truncate_empty_file_returns_none() {
        let f = NamedTempFile::new().unwrap();
        let mut rng = StdRng::seed_from_u64(1);
        assert!(truncate_file(f.path(), &mut rng).is_none());
    }

    #[test]
    fn truncate_file_fault_uses_path() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"data").unwrap();
        f.flush().unwrap();
        let mut fault = TruncateFile {
            path: f.path().to_path_buf(),
        };
        let mut rng = StdRng::seed_from_u64(1);
        fault.inject(&mut rng, &[]);
        let len = fs::metadata(f.path()).unwrap().len();
        assert!(len < 4);
    }
}
