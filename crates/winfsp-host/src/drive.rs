//! Automatic drive-letter selection.
//!
//! Mounting to a hard-coded letter (e.g. `Z:`) fails if that letter is
//! already taken (network share, `subst`, another mount). Like Windows
//! itself, we pick the FIRST FREE letter automatically so the mount "just
//! works". The bit-mask logic is pure and unit-tested here; the Windows
//! query of in-use letters is a thin `GetLogicalDrives` wrapper.

/// Preferred scan order for an auto-assigned mount: low letters first
/// (`D` → `Z`). Matches the Windows convention for removable media -
/// Explorer lists drives left-to-right, so a new USB lands next to
/// existing internal drives instead of at the far end. `A`/`B` (legacy
/// floppy) and `C` (system) are never auto-chosen.
const SCAN_ORDER: &[char] = &[
    'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V',
    'W', 'X', 'Y', 'Z',
];

/// First free drive letter given a `GetLogicalDrives` bit-mask
/// (bit 0 = `A:`, bit 1 = `B:`, … bit 25 = `Z:`; a set bit means the
/// letter is in use). Returns `None` if every candidate is taken.
pub fn first_free_drive_letter(used_mask: u32) -> Option<char> {
    SCAN_ORDER.iter().copied().find(|&c| {
        let bit = (c as u8 - b'A') as u32;
        used_mask & (1 << bit) == 0
    })
}

/// Bit-mask of in-use drive letters (`GetLogicalDrives`).
#[cfg(windows)]
pub fn used_drive_mask() -> u32 {
    // SAFETY: GetLogicalDrives takes no arguments and only returns a
    // bit-mask; it has no failure mode that requires a pointer/handle.
    unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() }
}

/// Resolve the mount point: an explicit request wins; otherwise the first
/// free drive letter as `"X:"`. `Err` only if nothing is free.
#[cfg(windows)]
pub fn resolve_mountpoint(explicit: Option<&str>) -> Result<String, String> {
    if let Some(m) = explicit {
        return Ok(m.to_string());
    }
    match first_free_drive_letter(used_drive_mask()) {
        Some(c) => Ok(format!("{c}:")),
        None => Err(
            "no free drive letter available (A-C reserved, D-Z all in use); \
             pass --mount with an explicit letter or directory path"
                .to_string(),
        ),
    }
}

// NOTE: an earlier revision routed bare drive letters through the WinFsp
// MountManager form (`\\?\X:`) to make an elevated mount visible to the
// non-elevated Explorer. Empirically that produced an unreadable volume on
// this setup ("the directory name is invalid" on the root), so it was
// reverted. Cross-elevation visibility is instead handled by the system-wide
// `EnableLinkedConnections=1` policy applied at install time.

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(c: char) -> u32 {
        1 << (c as u8 - b'A') as u32
    }

    #[test]
    fn picks_d_when_all_free() {
        assert_eq!(first_free_drive_letter(0), Some('D'));
    }

    #[test]
    fn skips_taken_low_letters() {
        let used = bit('D') | bit('E') | bit('F');
        assert_eq!(first_free_drive_letter(used), Some('G'));
    }

    #[test]
    fn never_returns_a_b_c_even_if_only_those_free() {
        // Everything D..=Z taken; only A,B,C "free" -> None (reserved).
        let mut used = 0u32;
        for c in 'D'..='Z' {
            used |= bit(c);
        }
        assert_eq!(first_free_drive_letter(used), None);
    }

    #[test]
    fn c_taken_does_not_block_selection() {
        // System drive C: in use (typical) must not affect the choice.
        assert_eq!(first_free_drive_letter(bit('C')), Some('D'));
    }

    #[test]
    fn falls_through_to_z_as_last_resort() {
        let mut used = 0u32;
        for c in 'D'..'Z' {
            used |= bit(c);
        }
        assert_eq!(first_free_drive_letter(used), Some('Z'));
    }

    #[test]
    fn none_when_every_candidate_taken() {
        let mut used = 0u32;
        for c in 'A'..='Z' {
            used |= bit(c);
        }
        assert_eq!(first_free_drive_letter(used), None);
    }
}
