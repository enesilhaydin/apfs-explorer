//! winfsp-host build script.
//!
//! winfsp-rs mandates delay-loading of `winfsp-x64.dll`. This emits the
//! linker flags so the WinFsp imports are delay-loaded; at runtime
//! `winfsp::winfsp_init()` calls `FspLoad`, which locates and loads the
//! DLL from the WinFsp install path recorded in the registry. Without
//! this, the binary fails to start with STATUS_DLL_NOT_FOUND (0xC0000135)
//! before `main` runs (the import would be a hard, non-delay dependency).
//!
//! Only relevant when building natively on Windows (host == target);
//! on macOS the `winfsp` build-dependency is absent (target-gated in
//! Cargo.toml) and this is a no-op.
fn main() {
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
