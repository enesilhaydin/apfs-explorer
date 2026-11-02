//! Many write operations to one checkpoint.
//!
//! Every operation used to run in a transaction of its own and commit at
//! once: a checkpoint, two durability barriers and a fresh reader per
//! operation, which is where nearly all the time of a metadata operation
//! went. macOS keeps a transaction open across operations and closes it on
//! thresholds and an idle timer; the volume on disk is always one
//! checkpoint or the next, and a power cut loses at most the operations of
//! the group still open.
//!
//! The reader's device is a [`GroupDev`]: the plain device while no group is
//! open (a read-only mount never opens one), or the open transaction, whose
//! reads lay what it has staged over the device. Each operation runs whole
//! or not at all ([`Transaction::atomically`]), so a failure reports its
//! own error and leaves the operations before it in the group; the reader
//! is then rebound to the superblock the operation staged, so the next
//! lookup sees it before it is committed. [`commit_group`] closes the group
//! and gives back a reader on the plain device.

use apfs::{FsError, FsView, VolumeSelector};
use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};
use apfs_write::txn::{Transaction, TxnError};
use std::time::{Duration, Instant};

/// The device a mounted volume's reader reads: plain, or through the open
/// write transaction.
pub enum GroupDev<D: WritableBlockDevice> {
    Plain(D),
    Open(Box<Transaction<D>>),
    /// Only while the device moves between the two.
    Gone,
}

impl<D: WritableBlockDevice> BlockDevice for GroupDev<D> {
    fn size(&self) -> u64 {
        match self {
            GroupDev::Plain(d) => d.size(),
            GroupDev::Open(t) => t.size(),
            GroupDev::Gone => 0,
        }
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        match self {
            GroupDev::Plain(d) => d.read_at(offset, buf),
            GroupDev::Open(t) => t.read_at(offset, buf),
            GroupDev::Gone => Err(BlockError::Io(std::io::Error::other(
                "the device is between a write transaction and the reader",
            ))),
        }
    }

    fn read_content_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        match self {
            GroupDev::Plain(d) => d.read_content_at(offset, buf),
            GroupDev::Open(t) => t.read_content_at(offset, buf),
            GroupDev::Gone => Err(BlockError::Io(std::io::Error::other(
                "the device is between a write transaction and the reader",
            ))),
        }
    }
}

/// Why a grouped write failed.
#[derive(Debug)]
pub enum GroupError {
    /// The operation itself, or opening the transaction.
    Txn(TxnError),
    /// Reading the volume through the transaction.
    Fs(FsError),
}

impl From<TxnError> for GroupError {
    fn from(e: TxnError) -> Self {
        GroupError::Txn(e)
    }
}

impl From<FsError> for GroupError {
    fn from(e: FsError) -> Self {
        GroupError::Fs(e)
    }
}

/// When an open group is closed.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// No operation for this long.
    pub idle: Duration,
    /// The group has been open this long.
    pub max_age: Duration,
    /// The group holds this many operations.
    pub max_ops: usize,
    /// The group holds this many bytes of blocks waiting to be written,
    /// file data included: they stay in memory until the commit.
    pub max_bytes: usize,
}

impl Default for Policy {
    /// Tighter than macOS, whose flusher closes the transaction every second
    /// on removable media: Windows treats an external disk as ready to pull
    /// out at any moment. A burst of operations (a copy, an unpack) still
    /// lands in one checkpoint; a pause of a tenth of a second closes it.
    /// Each commit waits on the disk twice, so the operation and byte caps
    /// are set to bound memory rather than to cut a burst short: the age
    /// cap already bounds what a power cut can lose.
    fn default() -> Self {
        Self {
            idle: Duration::from_millis(100),
            max_age: Duration::from_secs(1),
            max_ops: 4096,
            max_bytes: 64 << 20,
        }
    }
}

/// The open group's operation count, staged bytes and times.
#[derive(Debug, Default, Clone, Copy)]
pub struct GroupClock {
    pub ops: usize,
    bytes: usize,
    opened: Option<Instant>,
    last: Option<Instant>,
}

impl GroupClock {
    /// Whether a group with operations in it is open.
    pub fn is_open(&self) -> bool {
        self.ops > 0
    }

    /// Count one operation at `now`, after which the group stages
    /// `staged_bytes`.
    fn note(&mut self, now: Instant, staged_bytes: usize) {
        self.ops += 1;
        self.bytes = staged_bytes;
        self.opened.get_or_insert(now);
        self.last = Some(now);
    }

    /// Whether the group holds as much as `policy` lets it.
    pub fn full(&self, policy: &Policy) -> bool {
        self.ops >= policy.max_ops || self.bytes >= policy.max_bytes
    }

    /// Whether `policy` closes the group at `now`.
    pub fn due(&self, now: Instant, policy: &Policy) -> bool {
        if self.ops == 0 {
            return false;
        }
        let since =
            |t: Option<Instant>| t.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.full(policy) || since(self.last) >= policy.idle || since(self.opened) >= policy.max_age
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Run one write operation in `view`'s open group, opening a transaction
/// when none is open. The operation gets the volume superblock and
/// object-map header as the group has them; it lands whole or not at all,
/// and on success the reader is rebound so the next lookup sees it.
pub fn write_in_group<D: WritableBlockDevice, R>(
    view: &mut FsView<GroupDev<D>>,
    clock: &mut GroupClock,
    op: impl FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
) -> Result<R, GroupError> {
    write_in_group_as(view, clock, false, op)
}

/// [`write_in_group`] for an operation that adds, removes and renames no
/// entry: the reader keeps its cached paths and listings.
pub fn write_in_group_keeping_names<D: WritableBlockDevice, R>(
    view: &mut FsView<GroupDev<D>>,
    clock: &mut GroupClock,
    op: impl FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
) -> Result<R, GroupError> {
    write_in_group_as(view, clock, true, op)
}

fn write_in_group_as<D: WritableBlockDevice, R>(
    view: &mut FsView<GroupDev<D>>,
    clock: &mut GroupClock,
    keeps_names: bool,
    op: impl FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
) -> Result<R, GroupError> {
    if matches!(view.dev_mut(), GroupDev::Plain(_)) {
        match std::mem::replace(view.dev_mut(), GroupDev::Gone) {
            GroupDev::Plain(d) => match Transaction::begin_recoverable(d) {
                Ok(t) => *view.dev_mut() = GroupDev::Open(Box::new(t)),
                Err((d, e)) => {
                    *view.dev_mut() = GroupDev::Plain(d);
                    return Err(e.into());
                }
            },
            other => *view.dev_mut() = other,
        }
    }
    let (vsb, omap) = view.read_vsb_omap_raw()?;
    let vsb_oid = vsb
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| TxnError::VolumeParse("volume superblock shorter than its header".into()))?;
    let GroupDev::Open(txn) = view.dev_mut() else {
        return Err(TxnError::VolumeParse("no write transaction open".into()).into());
    };
    let out = txn.atomically(|t| op(t, &vsb, &omap))?;
    let staged = txn.staged_volume_block(vsb_oid);
    clock.note(Instant::now(), txn.staged_bytes());
    match staged {
        Some(p) if keeps_names => view.rebind_keeping_names(p)?,
        Some(p) => view.rebind(p)?,
        None => {}
    }
    Ok(out)
}

/// What closing a group gives back: the commit's outcome, and a reader on
/// the plain device.
pub type Committed<D> = (Result<(), TxnError>, Result<FsView<GroupDev<D>>, FsError>);

/// Close `view`'s open group: commit its transaction and open a reader on
/// the plain device. The commit's own outcome comes first; the reader is
/// reopened even when the commit failed, from the last durable checkpoint,
/// so the mount keeps serving.
pub fn commit_group<D: WritableBlockDevice>(
    view: FsView<GroupDev<D>>,
    sel: VolumeSelector,
    clock: &mut GroupClock,
) -> Committed<D> {
    clock.reset();
    let (committed, dev) = match view.into_dev() {
        GroupDev::Open(txn) => match (*txn).commit_recoverable() {
            Ok(d) => (Ok(()), d),
            Err((d, e)) => (Err(e), d),
        },
        GroupDev::Plain(d) => (Ok(()), d),
        GroupDev::Gone => {
            return (
                Err(TxnError::VolumeParse("the device was lost".into())),
                Err(FsError::NoVolume),
            )
        }
    };
    let mut dev = dev;
    dev.invalidate_read_cache();
    (committed, FsView::open_selected(GroupDev::Plain(dev), sel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_group_is_never_due() {
        let clock = GroupClock::default();
        assert!(!clock.due(Instant::now(), &Policy::default()));
    }

    #[test]
    fn a_group_closes_when_idle_old_or_full() {
        let policy = Policy {
            idle: Duration::from_millis(100),
            max_age: Duration::from_millis(300),
            max_ops: 3,
            max_bytes: usize::MAX,
        };
        let t0 = Instant::now();
        let mut clock = GroupClock::default();
        clock.note(t0, 0);
        assert!(!clock.due(t0 + Duration::from_millis(50), &policy));
        assert!(clock.due(t0 + Duration::from_millis(150), &policy), "idle");
        clock.note(t0 + Duration::from_millis(200), 0);
        assert!(!clock.due(t0 + Duration::from_millis(250), &policy));
        clock.note(t0 + Duration::from_millis(290), 0);
        assert!(clock.due(t0 + Duration::from_millis(295), &policy), "full");
        let mut clock = GroupClock::default();
        clock.note(t0, 0);
        clock.last = Some(t0 + Duration::from_millis(270));
        assert!(clock.due(t0 + Duration::from_millis(310), &policy), "old");
    }

    #[test]
    fn a_group_holding_too_many_bytes_is_full() {
        let policy = Policy {
            idle: Duration::from_secs(60),
            max_age: Duration::from_secs(60),
            max_ops: 100,
            max_bytes: 1 << 20,
        };
        let t0 = Instant::now();
        let mut clock = GroupClock::default();
        clock.note(t0, 4096);
        assert!(!clock.full(&policy));
        assert!(!clock.due(t0, &policy));
        // One large write staged its data: the group closes on it, however
        // few operations it holds.
        clock.note(t0, (1 << 20) + 4096);
        assert!(clock.full(&policy), "full by bytes");
        assert!(clock.due(t0, &policy));
        clock.reset();
        assert!(!clock.full(&policy), "an empty group holds nothing");
    }
}
