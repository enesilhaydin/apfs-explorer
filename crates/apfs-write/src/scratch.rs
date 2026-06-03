//! Pure scratch-image geometry helpers.
//!
//! A "scratch" image is a disposable APFS-in-GPT disk image used for write
//! testing. It is created by `xtask gen-scratch` (shells out to `hdiutil`) and
//! verified by `xtask verify-scratch` (shells out to `fsck_apfs -n`).
//!
//! This module contains only pure, no-I/O geometry logic so it can be unit-tested
//! without touching disk.

/// APFS block size is always 4096 bytes.
///
/// [CERTAIN: Apple APFS Reference "Block Size" §2.1; linux-apfs-rw `apfs_raw.h`
/// `#define APFS_BLOCK_SIZE 4096`]
pub const BLOCK_SIZE: u64 = 4_096;

/// Conservative minimum scratch-image size in bytes.
///
/// The APFS spec does not publish an absolute minimum container size, but empirical
/// observation (hdiutil + diskutil on macOS 13/14) shows that 32 MB is reliably
/// accepted. Anything below ~16 MB is rejected by `hdiutil -fs APFS`. We use 32 MB
/// as the conservative floor; Task 2 (`xtask gen-scratch`) confirms the exact
/// macOS-accepted minimum empirically - the kernel is the arbiter.
///
/// Value: 32 * 1024 * 1024 = 33_554_432 bytes.
pub const MIN_SCRATCH_BYTES: u64 = 32 * 1_024 * 1_024;

/// Geometry spec for a scratch image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchSpec {
    /// Image size in bytes, rounded up to the nearest [`BLOCK_SIZE`] multiple.
    pub size_bytes: u64,
    /// Block size in bytes (always [`BLOCK_SIZE`] = 4096).
    pub block_size: u64,
    /// Number of blocks.
    pub block_count: u64,
}

/// Errors returned by [`ScratchSpec::new`].
#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    /// Requested size is below the conservative APFS container floor.
    #[error("requested size {requested} bytes is below minimum scratch size {min} bytes")]
    TooSmall { requested: u64, min: u64 },
}

impl ScratchSpec {
    /// Build a [`ScratchSpec`] from a requested size in bytes.
    ///
    /// The actual `size_bytes` is rounded **up** to the nearest 4096-byte
    /// multiple. Returns [`ScratchError::TooSmall`] if the rounded size is
    /// still below [`MIN_SCRATCH_BYTES`].
    pub fn new(requested_bytes: u64) -> Result<Self, ScratchError> {
        // Round up to block boundary.
        let remainder = requested_bytes % BLOCK_SIZE;
        let size_bytes = if remainder == 0 {
            requested_bytes
        } else {
            requested_bytes + (BLOCK_SIZE - remainder)
        };

        if size_bytes < MIN_SCRATCH_BYTES {
            return Err(ScratchError::TooSmall {
                requested: requested_bytes,
                min: MIN_SCRATCH_BYTES,
            });
        }

        let block_count = size_bytes / BLOCK_SIZE;

        Ok(Self {
            size_bytes,
            block_size: BLOCK_SIZE,
            block_count,
        })
    }

    /// Convenience: build from a size in mebibytes (1 MiB = 1_048_576 bytes).
    pub fn from_mib(mib: u64) -> Result<Self, ScratchError> {
        Self::new(mib * 1_024 * 1_024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_size_is_4096() {
        let spec = ScratchSpec::from_mib(64).unwrap();
        assert_eq!(spec.block_size, 4_096);
    }

    #[test]
    fn size_is_multiple_of_block_size() {
        // Exact multiple - should stay unchanged.
        let exact = ScratchSpec::new(64 * 1_024 * 1_024).unwrap();
        assert_eq!(exact.size_bytes % BLOCK_SIZE, 0);

        // Non-multiple - should round up.
        let odd = ScratchSpec::new(64 * 1_024 * 1_024 + 1).unwrap();
        assert_eq!(odd.size_bytes % BLOCK_SIZE, 0);
        assert!(odd.size_bytes > 64 * 1_024 * 1_024 + 1);
    }

    #[test]
    fn block_count_matches_size() {
        let spec = ScratchSpec::from_mib(64).unwrap();
        assert_eq!(spec.size_bytes, spec.block_count * spec.block_size);
    }

    #[test]
    fn rejects_below_minimum() {
        // 1 MiB is well below the 32 MiB floor.
        let err = ScratchSpec::from_mib(1).unwrap_err();
        assert!(matches!(err, ScratchError::TooSmall { .. }));
    }

    #[test]
    fn rejects_zero() {
        let err = ScratchSpec::new(0).unwrap_err();
        assert!(matches!(err, ScratchError::TooSmall { .. }));
    }

    #[test]
    fn accepts_exact_minimum() {
        // 32 MiB is exactly the floor - must succeed.
        let spec = ScratchSpec::new(MIN_SCRATCH_BYTES).unwrap();
        assert_eq!(spec.size_bytes, MIN_SCRATCH_BYTES);
    }

    #[test]
    fn roundup_below_minimum_still_rejected() {
        // 1 byte rounded up to 4096 is still below 32 MiB.
        let err = ScratchSpec::new(1).unwrap_err();
        assert!(matches!(err, ScratchError::TooSmall { .. }));
    }

    #[test]
    fn from_mib_64_geometry() {
        let spec = ScratchSpec::from_mib(64).unwrap();
        assert_eq!(spec.size_bytes, 64 * 1_024 * 1_024);
        assert_eq!(spec.block_count, 64 * 1_024 * 1_024 / 4_096);
    }
}
