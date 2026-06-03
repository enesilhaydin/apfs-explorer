//! Batching consistency tests (M8 #2).
//!
//! These tests exercise `BatchQueue` directly - no real disk required.
//! They verify:
//!   1. 100 rename ops can be enqueued without panicking.
//!   2. Drain produces the exact ops in enqueue order.
//!   3. After flush (drain), queue is empty.
//!   4. Threshold-based auto-flush signals are correct.
//!   5. TruncateFast ops round-trip through the queue.
//!   6. SetInodeAttrs ops round-trip through the queue.
//!   7. MoveEntry ops round-trip through the queue.
//!
//! No `with_writable` is called here - that requires a real APFS image
//! and is covered by `integration_file_ops.rs` / chaos tests.

use winfsp_host::batch::{BatchQueue, DeferredOp, BATCH_OPS_MAX, BATCH_TIME_MS};

/// Enqueue 100 Rename ops and verify they drain in order.
#[test]
fn rename_batch_enqueue_drain_order() {
    let q = BatchQueue::new();
    for i in 0u64..100 {
        let op = DeferredOp::Rename {
            parent_ino: 2,
            old_name: format!("file_{i:04}"),
            new_name: format!("renamed_{i:04}"),
            replace_if_exists: false,
        };
        q.enqueue(op);
    }
    let ops = q.drain();
    assert_eq!(ops.len(), 100, "should have 100 pending ops");
    // Verify order preserved (FIFO).
    for (i, op) in ops.iter().enumerate() {
        match op {
            DeferredOp::Rename {
                old_name, new_name, ..
            } => {
                assert_eq!(*old_name, format!("file_{i:04}"));
                assert_eq!(*new_name, format!("renamed_{i:04}"));
            }
            other => panic!("unexpected op variant at index {i}: {other:?}"),
        }
    }
}

/// Drain after 100 ops leaves the queue empty.
#[test]
fn drain_leaves_empty_queue() {
    let q = BatchQueue::new();
    for i in 0u64..100 {
        q.enqueue(DeferredOp::Rename {
            parent_ino: 2,
            old_name: format!("a{i}"),
            new_name: format!("b{i}"),
            replace_if_exists: false,
        });
    }
    let drained = q.drain();
    assert_eq!(drained.len(), 100);
    assert!(!q.has_pending(), "queue must be empty after drain");
    assert!(q.drain().is_empty(), "second drain must yield nothing");
}

/// The 64th enqueue (BATCH_OPS_MAX) signals flush.
#[test]
fn threshold_flush_signal_at_max() {
    let q = BatchQueue::new();
    let mut signalled = false;
    for i in 0..BATCH_OPS_MAX {
        let needs = q.enqueue(DeferredOp::Rename {
            parent_ino: 2,
            old_name: format!("o{i}"),
            new_name: format!("n{i}"),
            replace_if_exists: false,
        });
        if needs {
            assert_eq!(
                i,
                BATCH_OPS_MAX - 1,
                "flush must signal exactly at index {}",
                BATCH_OPS_MAX - 1
            );
            signalled = true;
        }
    }
    assert!(
        signalled,
        "flush signal must have fired at {BATCH_OPS_MAX} ops"
    );
}

/// Ops below threshold do not signal flush.
#[test]
fn below_threshold_no_signal() {
    let q = BatchQueue::new();
    for i in 0..(BATCH_OPS_MAX - 1) {
        let needs = q.enqueue(DeferredOp::Rename {
            parent_ino: 2,
            old_name: format!("o{i}"),
            new_name: format!("n{i}"),
            replace_if_exists: false,
        });
        assert!(!needs, "op {i} must not trigger flush below threshold");
    }
}

/// TruncateFast ops round-trip correctly through the queue.
#[test]
fn truncate_fast_ops_roundtrip() {
    let q = BatchQueue::new();
    q.enqueue(DeferredOp::TruncateFast {
        parent_ino: 5,
        name: "bigfile.dat".into(),
        path: "/bigfile.dat".into(),
        new_size: 0,
        current_size: 1_048_576,
    });
    q.enqueue(DeferredOp::TruncateFast {
        parent_ino: 5,
        name: "growing.dat".into(),
        path: "/growing.dat".into(),
        new_size: 4096,
        current_size: 0,
    });
    let ops = q.drain();
    assert_eq!(ops.len(), 2);
    match &ops[0] {
        DeferredOp::TruncateFast {
            name,
            new_size,
            current_size,
            ..
        } => {
            assert_eq!(name, "bigfile.dat");
            assert_eq!(*new_size, 0);
            assert_eq!(*current_size, 1_048_576);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match &ops[1] {
        DeferredOp::TruncateFast { name, new_size, .. } => {
            assert_eq!(name, "growing.dat");
            assert_eq!(*new_size, 4096);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// SetInodeAttrs ops round-trip correctly through the queue.
#[test]
fn set_inode_attrs_roundtrip() {
    let q = BatchQueue::new();
    q.enqueue(DeferredOp::SetInodeAttrs {
        parent_ino: 3,
        name: "doc.txt".into(),
        mode_set: Some(0o200),
        mode_clear: None,
        mtime_ns: Some(1_700_000_000_000_000_000),
        ctime_ns: None,
        atime_ns: None,
        btime_ns: None,
        bsd_flags_set: None,
        bsd_flags_clear: None,
    });
    let ops = q.drain();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        DeferredOp::SetInodeAttrs {
            parent_ino,
            name,
            mode_set,
            mtime_ns,
            ..
        } => {
            assert_eq!(*parent_ino, 3);
            assert_eq!(name, "doc.txt");
            assert_eq!(*mode_set, Some(0o200));
            assert_eq!(*mtime_ns, Some(1_700_000_000_000_000_000));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// MoveEntry ops round-trip correctly through the queue.
#[test]
fn move_entry_roundtrip() {
    let q = BatchQueue::new();
    q.enqueue(DeferredOp::MoveEntry {
        old_parent_ino: 10,
        old_name: "src.txt".into(),
        new_parent_ino: 20,
        new_name: "dst.txt".into(),
        replace_if_exists: true,
    });
    let ops = q.drain();
    assert_eq!(ops.len(), 1);
    match &ops[0] {
        DeferredOp::MoveEntry {
            old_parent_ino,
            old_name,
            new_parent_ino,
            new_name,
            replace_if_exists,
        } => {
            assert_eq!(*old_parent_ino, 10);
            assert_eq!(old_name, "src.txt");
            assert_eq!(*new_parent_ino, 20);
            assert_eq!(new_name, "dst.txt");
            assert!(*replace_if_exists);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// Mixed op types enqueue and drain in FIFO order.
#[test]
fn mixed_ops_fifo_order() {
    let q = BatchQueue::new();
    q.enqueue(DeferredOp::Rename {
        parent_ino: 2,
        old_name: "a".into(),
        new_name: "b".into(),
        replace_if_exists: false,
    });
    q.enqueue(DeferredOp::TruncateFast {
        parent_ino: 2,
        name: "b".into(),
        path: "/b".into(),
        new_size: 0,
        current_size: 100,
    });
    q.enqueue(DeferredOp::SetInodeAttrs {
        parent_ino: 2,
        name: "b".into(),
        mode_set: None,
        mode_clear: None,
        mtime_ns: Some(999),
        ctime_ns: None,
        atime_ns: None,
        btime_ns: None,
        bsd_flags_set: None,
        bsd_flags_clear: None,
    });
    let ops = q.drain();
    assert_eq!(ops.len(), 3);
    assert!(matches!(&ops[0], DeferredOp::Rename { .. }));
    assert!(matches!(&ops[1], DeferredOp::TruncateFast { .. }));
    assert!(matches!(&ops[2], DeferredOp::SetInodeAttrs { .. }));
}

/// BATCH_TIME_MS constant is sane (> 0, <= 1000 ms).
#[test]
fn time_threshold_constant_is_sane() {
    assert!(BATCH_TIME_MS > 0, "BATCH_TIME_MS must be positive");
    assert!(
        BATCH_TIME_MS <= 1000,
        "BATCH_TIME_MS should not exceed 1 second"
    );
}
