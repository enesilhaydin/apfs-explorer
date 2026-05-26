#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! apfs-tray — user-session auto-mount agent + tray for APFS volumes.
//!
//! Cross-platform (unit-tested on macOS): [`supervisor`].
//! Windows-only (`#[cfg(windows)]`): device watch, child mgmt, tray.

pub mod config;
pub mod supervisor;

#[cfg(windows)]
pub mod child;
#[cfg(windows)]
pub mod detect;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod winwin;

#[cfg(test)]
mod skeleton_tests {
    #[test]
    fn crate_compiles_cross_platform() {
        assert_eq!(2 + 2, 4);
    }
}
