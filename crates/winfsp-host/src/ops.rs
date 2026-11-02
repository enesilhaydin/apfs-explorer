//! The catalog operations the host carries out for Windows: a rename, a move,
//! an attribute change or a metadata-only resize, each applied the moment
//! the request arrives, in the mount's open write group.
//!
//! They used to be queued and replayed later in one batch. A batch could not
//! give Windows the outcome of an operation while the request was still open
//! (a rename onto an existing name failed inside a later flush, after Windows
//! had been told it succeeded). So every operation is applied at once and
//! reports its own result; only the checkpoint that makes a group of them
//! durable comes later (see `crate::group`).

use apfs_core::block_device::WritableBlockDevice;
use apfs_write::file as apfs_write_file;
use apfs_write::txn::{Transaction, TxnError};

/// One catalog operation with everything needed to carry it out.
#[derive(Debug)]
pub enum HostOp {
    Rename {
        parent_ino: u64,
        old_name: String,
        new_name: String,
        replace_if_exists: bool,
    },
    MoveEntry {
        old_parent_ino: u64,
        old_name: String,
        new_parent_ino: u64,
        new_name: String,
        replace_if_exists: bool,
    },
    SetInodeAttrs {
        parent_ino: u64,
        name: String,
        mode_set: Option<u16>,
        mode_clear: Option<u16>,
        mtime_ns: Option<u64>,
        ctime_ns: Option<u64>,
        atime_ns: Option<u64>,
        btime_ns: Option<u64>,
        bsd_flags_set: Option<u32>,
        bsd_flags_clear: Option<u32>,
    },
    TruncateFast {
        parent_ino: u64,
        name: String,
        /// Full Unix-style path (e.g. `/dir/file`), for the read-and-rewrite
        /// path taken when the metadata-only resize declines (a cut inside a
        /// block, a shared or compressed stream).
        path: String,
        new_size: u64,
        current_size: u64,
    },
}

/// What applying one operation produced.
pub enum OpOutcome {
    Ok,
    Truncate(apfs_write_file::TruncateOutcome),
}

/// Carry out `op` inside an already-open transaction.
///
/// A `TruncateFast` outcome of `FastPathDeclined` means nothing was changed
/// and the caller has to rewrite the file itself.
pub fn apply_op<D: WritableBlockDevice>(
    op: &HostOp,
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
) -> Result<OpOutcome, TxnError> {
    match op {
        HostOp::Rename {
            parent_ino,
            old_name,
            new_name,
            replace_if_exists,
        } => {
            apfs_write_file::rename(
                txn,
                vsb_raw,
                omap_raw,
                *parent_ino,
                old_name,
                new_name,
                *replace_if_exists,
            )?;
            Ok(OpOutcome::Ok)
        }
        HostOp::MoveEntry {
            old_parent_ino,
            old_name,
            new_parent_ino,
            new_name,
            replace_if_exists,
        } => {
            apfs_write_file::move_entry(
                txn,
                vsb_raw,
                omap_raw,
                *old_parent_ino,
                old_name,
                *new_parent_ino,
                new_name,
                *replace_if_exists,
            )?;
            Ok(OpOutcome::Ok)
        }
        HostOp::SetInodeAttrs {
            parent_ino,
            name,
            mode_set,
            mode_clear,
            mtime_ns,
            ctime_ns,
            atime_ns,
            btime_ns,
            bsd_flags_set,
            bsd_flags_clear,
        } => {
            apfs_write_file::set_inode_attrs(
                txn,
                vsb_raw,
                omap_raw,
                *parent_ino,
                name,
                *mode_set,
                *mode_clear,
                *mtime_ns,
                *ctime_ns,
                *atime_ns,
                *btime_ns,
                *bsd_flags_set,
                *bsd_flags_clear,
            )?;
            Ok(OpOutcome::Ok)
        }
        HostOp::TruncateFast {
            parent_ino,
            name,
            path: _,
            new_size,
            current_size,
        } => {
            let outcome = apfs_write_file::truncate_file_fast(
                txn,
                vsb_raw,
                omap_raw,
                *parent_ino,
                name,
                *new_size,
                *current_size,
            )?;
            Ok(OpOutcome::Truncate(outcome))
        }
    }
}
