//! Opt-in counters of what the mount does: how often each callback runs and
//! how long it takes, and how many groups are committed. Set `APFS_STATS` to
//! a file path; the counts are written there when the mount stops. Off, a
//! counter costs one load of a flag.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static ON: AtomicBool = AtomicBool::new(false);
static PATH: OnceLock<Option<String>> = OnceLock::new();
static COUNTS: Mutex<BTreeMap<&'static str, (u64, u128)>> = Mutex::new(BTreeMap::new());

fn enabled() -> bool {
    let path = PATH.get_or_init(|| std::env::var("APFS_STATS").ok());
    if path.is_some() {
        ON.store(true, Ordering::Relaxed);
    }
    ON.load(Ordering::Relaxed)
}

/// Counts one event and, when dropped, the time since it was made.
pub struct Timed {
    name: &'static str,
    start: Option<Instant>,
}

/// Start timing one `name` event.
pub fn timed(name: &'static str) -> Timed {
    Timed {
        name,
        start: enabled().then(Instant::now),
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        let Some(start) = self.start else {
            return;
        };
        let ns = start.elapsed().as_nanos();
        if let Ok(mut c) = COUNTS.lock() {
            let e = c.entry(self.name).or_insert((0, 0));
            e.0 += 1;
            e.1 += ns;
        }
    }
}

/// Write the counts to the `APFS_STATS` file, one line per event: name,
/// count, total milliseconds.
pub fn dump() {
    let Some(Some(path)) = PATH.get() else {
        return;
    };
    let Ok(c) = COUNTS.lock() else {
        return;
    };
    let mut out = String::new();
    for (name, (n, ns)) in c.iter() {
        out.push_str(&format!(
            "{name:<24} {n:>8} {:>10.1} ms\n",
            *ns as f64 / 1e6
        ));
    }
    if let Err(e) = std::fs::write(path, out) {
        eprintln!("stats: cannot write {path}: {e}");
    }
}
