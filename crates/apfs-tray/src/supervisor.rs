//! Pure device→mount supervisor.
//!
//! Inputs: the set of APFS physical-drive numbers currently present, and
//! liveness of the child we spawned per drive. Output: the actions the
//! Windows layer must perform (spawn / kill). No Windows or process code
//! here so it is exhaustively unit-tested on any OS.

use std::collections::BTreeMap;

/// What the Windows layer should do for one drive this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Spawn `apfs-mount` for this physical drive number.
    Mount(u32),
    /// Kill the child for this physical drive number (device gone, or quit).
    Unmount(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Child spawned and believed alive.
    Mounted,
    /// Child died unexpectedly while the device is still present.
    Crashed { restarts: u32 },
}

/// Tracks intended mounts and produces actions. The Windows layer feeds it
/// observations (`present`, `child_alive`) and executes returned actions.
#[derive(Debug, Default)]
pub struct Supervisor {
    drives: BTreeMap<u32, State>,
}

/// Max automatic restarts before giving up on a crash-looping mount.
pub const MAX_RESTARTS: u32 = 3;

impl Supervisor {
    pub const fn new() -> Self {
        Self {
            drives: BTreeMap::new(),
        }
    }

    /// Reconcile against the present APFS drive set. `present` = drive
    /// numbers detected now. Returns the actions to perform.
    pub fn reconcile(&mut self, present: &[u32]) -> Vec<Action> {
        let mut actions = Vec::new();
        // Removed devices: kill child, forget.
        let gone: Vec<u32> = self
            .drives
            .keys()
            .copied()
            .filter(|d| !present.contains(d))
            .collect();
        for d in gone {
            self.drives.remove(&d);
            actions.push(Action::Unmount(d));
        }
        // New devices: mount.
        for &d in present {
            if let std::collections::btree_map::Entry::Vacant(e) = self.drives.entry(d) {
                e.insert(State::Mounted);
                actions.push(Action::Mount(d));
            }
        }
        actions
    }

    /// Watchdog tick: the Windows layer reports, for each tracked drive,
    /// whether its child is still alive. A dead child for a still-present
    /// device is respawned up to `MAX_RESTARTS`.
    pub fn watchdog(&mut self, alive: &dyn Fn(u32) -> bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let drives: Vec<u32> = self.drives.keys().copied().collect();
        for d in drives {
            if alive(d) {
                if let Some(s) = self.drives.get_mut(&d) {
                    *s = State::Mounted;
                }
                continue;
            }
            let restarts = match self.drives.get(&d) {
                Some(State::Crashed { restarts }) => *restarts,
                _ => 0,
            };
            if restarts >= MAX_RESTARTS {
                continue; // give up; leave tracked but no more respawns
            }
            self.drives.insert(
                d,
                State::Crashed {
                    restarts: restarts + 1,
                },
            );
            actions.push(Action::Mount(d));
        }
        actions
    }

    /// Drives we currently believe are mounted (for the tray menu).
    pub fn tracked(&self) -> Vec<u32> {
        self.drives.keys().copied().collect()
    }

    /// Remove a single drive from tracking after a user-initiated eject.
    /// Does not produce an `Action` — the caller already killed the child.
    pub fn forget(&mut self, drive: u32) {
        self.drives.remove(&drive);
    }

    /// Quit: unmount everything.
    pub fn shutdown(&mut self) -> Vec<Action> {
        let actions = self.drives.keys().map(|&d| Action::Unmount(d)).collect();
        self.drives.clear();
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_new_unmounts_removed() {
        let mut s = Supervisor::new();
        assert_eq!(s.reconcile(&[3]), vec![Action::Mount(3)]);
        assert_eq!(s.reconcile(&[3]), vec![]);
        assert_eq!(s.reconcile(&[3, 5]), vec![Action::Mount(5)]);
        assert_eq!(s.reconcile(&[5]), vec![Action::Unmount(3)]);
        assert_eq!(s.tracked(), vec![5]);
    }

    #[test]
    fn watchdog_respawns_dead_child_up_to_limit() {
        let mut s = Supervisor::new();
        s.reconcile(&[3]);
        let dead = |_d: u32| false;
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&dead), vec![]); // MAX_RESTARTS reached
    }

    #[test]
    fn watchdog_noop_when_alive() {
        let mut s = Supervisor::new();
        s.reconcile(&[3]);
        let alive = |_d: u32| true;
        assert_eq!(s.watchdog(&alive), vec![]);
    }

    #[test]
    fn alive_again_resets_crash_count() {
        let mut s = Supervisor::new();
        s.reconcile(&[3]);
        let dead = |_d: u32| false;
        let alive = |_d: u32| true;
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&alive), vec![]);
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
    }

    #[test]
    fn shutdown_unmounts_all() {
        let mut s = Supervisor::new();
        s.reconcile(&[3, 5]);
        let mut a = s.shutdown();
        a.sort_by_key(|x| match x {
            Action::Unmount(d) | Action::Mount(d) => *d,
        });
        assert_eq!(a, vec![Action::Unmount(3), Action::Unmount(5)]);
        assert!(s.tracked().is_empty());
    }
}
