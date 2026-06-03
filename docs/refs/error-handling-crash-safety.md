# Error Handling, Crash Safety, and Logging

This project parses **untrusted disk data** and runs behind File Explorer.
Crash safety is not optional - it is a **core requirement**.

## Hard rules

1. **`unwrap()`, `expect()`, `panic!()`, `unreachable!()`, `todo!()`, and
   index-out-of-bounds panics are forbidden in library code paths.** Every
   error is propagated via `Result<_, ApfsError>`. (`apfs-core`, `apfs`,
   `apfs-write`)
2. Error types: one `thiserror` enum per crate (`ApfsError`, `WriteError`, …);
   upper layers wrap with `#[from]`. Variants must be meaningful (include
   offset, expected value, actual value).
3. **Error swallowing is forbidden:** no empty `catch`, `let _ = result`, or
   silent drops via `if let Ok(_)`. Either propagate with `?` or log with
   `tracing::warn!` / `tracing::error!`.
4. **Panic barriers:** use `std::panic::catch_unwind` at external boundaries:
   - Every WinFsp callback (a corrupt file must not bring down the volume)
   - The service task loop / every device-event handler
   - FFI / Win32 call wrappers
   `catch_unwind` logs what it caught via `error!` and returns a safe error code.
5. **Corrupt, truncated, or malicious images must produce a graceful `Err`,
   never a panic.** This is verified with fuzzing (`cargo-fuzz` targets:
   superblock, btree, extent, decmpfs, keybag parsing). Fuzz smoke runs in CI.
6. **Clippy gate (CI `-D` flags):** `clippy::unwrap_used`, `expect_used`,
   `panic`, `indexing_slicing` (in lib crates), `unwrap_in_result`.
   `cargo deny` enforces licenses and RUSTSEC advisories.
   `#![forbid(unsafe_code)]` in `apfs-core`; in other crates `unsafe` is
   allowed only in justified FFI / raw-IO blocks and requires a `// SAFETY:`
   comment.

## Logging

- `tracing` + `tracing-subscriber`. File output via `tracing-appender` with
  daily rotation.
- Log level is configured in TOML; default is `info`. Use `debug` / `trace`
  for troubleshooting.
- The service also writes critical events to the **Windows Event Log** (mount
  failure, watchdog restart, validation error).
- Structured fields: `volume_id`, `oid`, `xid`, `path`. File contents are
  never logged - only metadata (to avoid PII leakage).
- Write path (v2): every write operation is recorded in the audit log with the
  pre-operation snapshot ID and the post-operation differential result.

## Service resilience

- Watchdog: if the service main loop panics or exits, it is restarted
  automatically (SCM recovery + internal supervisor). On restart the last
  known mount state is reconstructed from persistent state.
- Mount loss triggers a tray notification and an Event Log entry.
- Graceful shutdown: on SCM stop, all volumes are cleanly unmounted before
  the process exits.

## Read-only guarantee (v1)

Write / Create / Rename callbacks are **not implemented at the code level**
(they return `STATUS_MEDIA_WRITE_PROTECTED` only). The compiler enforces
read-only; when a user attempts a write they receive a clear "read-only media"
error (tray notification + Explorer error dialog).

## Test threshold

No milestone is considered complete until the relevant crate (a) passes the
clippy gate, (b) passes corrupt-image tests and fuzz smoke, and (c) holds up
against a real USB differential test (M4+).
