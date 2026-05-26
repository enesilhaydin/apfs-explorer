//! End-to-end of the PURE supervisor against a scripted device timeline
//! (no Windows): plug 3 → plug 5 → crash 3 (respawn) → unplug 5 → quit.

use apfs_tray::supervisor::{Action, Supervisor};

#[test]
fn full_lifecycle_timeline() {
    let mut s = Supervisor::new();
    assert_eq!(s.reconcile(&[3]), vec![Action::Mount(3)]);
    assert_eq!(s.reconcile(&[3, 5]), vec![Action::Mount(5)]);

    // Child 3 died, 5 alive → only 3 respawns.
    let alive = |d: u32| d == 5;
    assert_eq!(s.watchdog(&alive), vec![Action::Mount(3)]);

    // 5 unplugged.
    assert_eq!(s.reconcile(&[3]), vec![Action::Unmount(5)]);
    assert_eq!(s.tracked(), vec![3]);

    // Quit.
    assert_eq!(s.shutdown(), vec![Action::Unmount(3)]);
    assert!(s.tracked().is_empty());
}
