//! Duplicate detection based on stable file fingerprints.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::error::{Result, VaultError};

const FNV64_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Chunk size for streaming file hashing. Kept small so large media files
/// (multi-GB videos) never get fully loaded into memory during a scan. (#7)
const FINGERPRINT_CHUNK_SIZE: usize = 64 * 1024;

/// Stable hash information for a file or byte slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    /// Name of the hashing algorithm.
    pub algorithm: &'static str,
    /// Hex-encoded fingerprint value.
    pub hash: String,
    /// Size of the content in bytes.
    pub byte_len: u64,
}

impl FileFingerprint {
    /// Creates a new fingerprint instance.
    pub fn new(hash: String, byte_len: u64) -> Self {
        Self {
            algorithm: "fnv1a64",
            hash,
            byte_len,
        }
    }
}

/// Computes a stable fingerprint for the provided bytes.
pub fn compute_fingerprint(bytes: &[u8]) -> FileFingerprint {
    let mut state = FNV64_OFFSET_BASIS;

    for byte in bytes {
        state ^= u64::from(*byte);
        state = state.wrapping_mul(FNV64_PRIME);
    }

    FileFingerprint::new(format!("{state:016x}"), bytes.len() as u64)
}

/// Computes a fingerprint for a file on disk.
///
/// The file is read in fixed-size chunks and folded incrementally, so memory
/// usage stays constant regardless of file size. Because FNV-1a is a
/// sequential byte fold, the result is identical to hashing the whole file at
/// once via [`compute_fingerprint`].
pub fn compute_fingerprint_for_file(path: impl AsRef<Path>) -> Result<FileFingerprint> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    let mut state = FNV64_OFFSET_BASIS;
    let mut byte_len: u64 = 0;
    let mut buffer = vec![0u8; FINGERPRINT_CHUNK_SIZE];

    loop {
        let read = reader.read(&mut buffer).map_err(VaultError::from)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            state ^= u64::from(*byte);
            state = state.wrapping_mul(FNV64_PRIME);
        }
        byte_len += read as u64;
    }

    Ok(FileFingerprint::new(format!("{state:016x}"), byte_len))
}

/// Returns `true` when two fingerprints describe identical content.
pub fn is_same_file(a: &FileFingerprint, b: &FileFingerprint) -> bool {
    a.hash == b.hash && a.byte_len == b.byte_len && a.algorithm == b.algorithm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_stable_for_same_bytes() {
        let first = compute_fingerprint(b"duplicate");
        let second = compute_fingerprint(b"duplicate");
        assert_eq!(first, second);
    }

    #[test]
    fn fingerprints_differ_for_different_bytes() {
        let first = compute_fingerprint(b"one");
        let second = compute_fingerprint(b"two");
        assert!(!is_same_file(&first, &second));
    }

    #[test]
    fn streaming_file_hash_matches_in_memory_hash() {
        use std::io::Write;

        // Use content larger than one chunk to exercise the streaming loop.
        let content: Vec<u8> = (0..(FINGERPRINT_CHUNK_SIZE * 2 + 123))
            .map(|i| (i % 251) as u8)
            .collect();

        let mut path = std::env::temp_dir();
        path.push(format!("fero_fp_test_{}.bin", std::process::id()));
        std::fs::File::create(&path)
            .and_then(|mut f| f.write_all(&content))
            .expect("temp fixture should be writable");

        let from_file = compute_fingerprint_for_file(&path).expect("file hash should succeed");
        let from_memory = compute_fingerprint(&content);

        // Streaming in chunks must yield the exact same fingerprint as a
        // single-buffer hash (byte count and FNV state alike).
        assert_eq!(from_file, from_memory);

        let _ = std::fs::remove_file(&path);
    }
}
