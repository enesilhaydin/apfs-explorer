//! Write guards: refuse mutations whose on-disk bookkeeping we do not
//! implement, instead of performing them and losing user data.
//!
//! The guard that remains is volume-level: a sealed or encrypted volume is
//! state we cannot maintain, so it is refused. The former guards on hard-linked
//! and compressed files are gone, because those operations are now performed
//! correctly rather than refused (see `hard_links.rs`, `decmpfs_write.rs` and
//! `decmpfs_truncate.rs`, which prove them against real images).
//!
//! These are pure predicate tests: no image is needed, which is what lets them
//! run on every platform.
#![cfg(feature = "write")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use apfs_write::file::ensure_volume_mutable;

const VSBI_INCOMPAT_FEATURES: usize = 0x38;
const VSBI_FS_FLAGS: usize = 0x108;
const APFS_INCOMPAT_SEALED: u64 = 0x20;
const APFS_FS_UNENCRYPTED: u64 = 0x1;

/// Minimal volume superblock bytes carrying the two fields the guard reads.
fn vsb(incompat: u64, fs_flags: u64) -> Vec<u8> {
    let mut v = vec![0u8; 4096];
    v[VSBI_INCOMPAT_FEATURES..VSBI_INCOMPAT_FEATURES + 8].copy_from_slice(&incompat.to_le_bytes());
    v[VSBI_FS_FLAGS..VSBI_FS_FLAGS + 8].copy_from_slice(&fs_flags.to_le_bytes());
    v
}

#[test]
fn plain_volume_is_mutable() {
    // Unencrypted, unsealed: the ordinary case must stay writable.
    ensure_volume_mutable(&vsb(0x1, APFS_FS_UNENCRYPTED)).expect("plain volume must be writable");
}

#[test]
fn sealed_volume_is_refused() {
    let e = ensure_volume_mutable(&vsb(APFS_INCOMPAT_SEALED | 0x1, APFS_FS_UNENCRYPTED))
        .expect_err("sealed volume must be refused");
    assert!(
        format!("{e}").contains("sealed"),
        "error should name the reason, got: {e}"
    );
}

#[test]
fn encrypted_volume_is_refused() {
    // APFS_FS_UNENCRYPTED clear => FileVault. Writing plaintext extents into it
    // would produce content macOS cannot decrypt.
    let e = ensure_volume_mutable(&vsb(0x1, 0)).expect_err("encrypted volume must be refused");
    assert!(
        format!("{e}").contains("encrypted"),
        "error should name the reason, got: {e}"
    );
}

#[test]
fn guards_reject_truncated_values() {
    // A short value must not panic or silently pass the guard.
    assert!(ensure_volume_mutable(&[0u8; 8]).is_err());
}
