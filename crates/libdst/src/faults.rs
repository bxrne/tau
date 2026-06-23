//! Fault injection primitives.
//!
//! [`Fault`] is the extension point; [`TruncateFile`] and [`CorruptFile`] are the
//! built-in implementations modelling the two ways persisted bytes go bad: a
//! short write (truncation) and bit-rot / a torn write (corruption). Implement
//! [`Fault`] for additional fault types (partial writes, etc.).

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rand::Rng;
use rand::rngs::StdRng;

/// Largest contiguous run of bytes a single [`corrupt_file`] call flips.
const MAX_CORRUPT_RUN: u64 = 16;

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

/// Flip a contiguous run of bytes in a specific file (bit-rot / torn write).
pub struct CorruptFile {
    pub path: PathBuf,
}

impl Fault for CorruptFile {
    fn name(&self) -> &str {
        "corrupt_file"
    }

    fn inject(&mut self, rng: &mut StdRng, _paths: &[&Path]) {
        corrupt_file(&self.path, rng);
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

/// Corrupt a file in place by flipping a short contiguous run of bytes at a
/// uniformly random offset. Each byte is XORed with a non-zero mask, so the file
/// is guaranteed to change (no accidental no-op). The file length is preserved —
/// this models bit-rot or a torn write rather than a short write.
///
/// Returns the number of bytes corrupted, or `None` if the file was empty or
/// missing.
pub fn corrupt_file(path: &Path, rng: &mut StdRng) -> Option<u64> {
    let len = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return None;
    }
    let offset = rng.gen_range(0..len);
    let run = rng.gen_range(1..=MAX_CORRUPT_RUN.min(len - offset));
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open for corruption");
    let mut buf = vec![0u8; run as usize];
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.read_exact(&mut buf).expect("read run");
    for b in &mut buf {
        // Non-zero mask guarantees the byte actually changes.
        *b ^= rng.gen_range(1..=u8::MAX);
    }
    file.seek(SeekFrom::Start(offset)).expect("seek back");
    file.write_all(&buf).expect("write corruption");
    file.flush().expect("flush corruption");
    Some(run)
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

    #[test]
    fn corrupt_file_changes_bytes_without_resizing() {
        let original = b"hello world 1234567890".to_vec();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&original).unwrap();
        f.flush().unwrap();
        let mut rng = StdRng::seed_from_u64(7);
        let run = corrupt_file(f.path(), &mut rng);
        assert!(run.is_some());
        let n = run.unwrap();
        assert!((1..=MAX_CORRUPT_RUN).contains(&n));
        let after = fs::read(f.path()).unwrap();
        assert_eq!(after.len(), original.len(), "length must be preserved");
        assert_ne!(after, original, "contents must change");
        // Exactly `n` contiguous bytes differ.
        let diffs = original.iter().zip(&after).filter(|(a, b)| a != b).count();
        assert_eq!(diffs as u64, n);
    }

    #[test]
    fn corrupt_empty_file_returns_none() {
        let f = NamedTempFile::new().unwrap();
        let mut rng = StdRng::seed_from_u64(1);
        assert!(corrupt_file(f.path(), &mut rng).is_none());
    }

    #[test]
    fn corrupt_file_is_deterministic_for_a_seed() {
        let original = b"deterministic corruption payload".to_vec();
        let render = |seed: u64| {
            let mut f = NamedTempFile::new().unwrap();
            f.write_all(&original).unwrap();
            f.flush().unwrap();
            let mut rng = StdRng::seed_from_u64(seed);
            corrupt_file(f.path(), &mut rng);
            fs::read(f.path()).unwrap()
        };
        assert_eq!(render(42), render(42), "same seed -> same corruption");
    }

    #[test]
    fn corrupt_file_fault_uses_path() {
        let original = b"payload bytes".to_vec();
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&original).unwrap();
        f.flush().unwrap();
        let mut fault = CorruptFile {
            path: f.path().to_path_buf(),
        };
        let mut rng = StdRng::seed_from_u64(3);
        fault.inject(&mut rng, &[]);
        assert_ne!(fs::read(f.path()).unwrap(), original);
    }
}
