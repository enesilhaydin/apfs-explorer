#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! apfs-write - copy-on-write mutation. Behind the `write` feature, verification-first.

pub mod file;
pub(crate) mod free_runs;
pub mod scratch;
pub mod sm_fq;
pub mod snapshot;
pub mod txn;
