//! Enumerate physical drives and return those whose GPT contains an APFS
//! container. Thin orchestration over the proven winfsp-host primitives —
//! no APFS parsing is re-implemented here.

use winfsp_host::gpt::find_apfs_container;
use winfsp_host::winraw::PhysicalDrive;

/// Physical-drive numbers (`\\.\PhysicalDriveN`) that currently hold an
/// APFS container. Scans 0..16 (same bound as `autodetect_apfs`).
pub fn present_apfs_drives() -> Vec<u32> {
    let mut out = Vec::new();
    for n in 0..16u32 {
        if let Ok(mut d) = PhysicalDrive::open(n) {
            if find_apfs_container(&mut d).is_ok() {
                out.push(n);
            }
        }
    }
    out
}
