#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! winfsp-host - mount an APFS container as a Windows drive letter.
//!
//! Cross-platform (unit-tested on macOS): [`align`], [`cache`], [`gpt`],
//! [`offset_dev`]. Windows-only (`#[cfg(windows)]`): raw physical device +
//! winfsp callbacks.

pub mod align;
pub mod cache;
pub mod drive;
pub use apfs_core::gpt;
pub mod group;
pub mod image_mount;
pub mod logfile;
pub mod offset_dev;
pub mod ops;
pub mod stats;
pub mod subcmd;

#[cfg(windows)]
pub mod mount;
#[cfg(windows)]
pub mod winraw;

#[cfg(test)]
mod skeleton_tests {
    #[test]
    fn crate_compiles_cross_platform() {
        assert_eq!(2 + 2, 4);
    }
}
