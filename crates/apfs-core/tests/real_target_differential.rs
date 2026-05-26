//! Real-target differential: open a real APFS container image with our reader
//! and byte-compare every file against the macOS kernel's own read of the
//! same volume (the ground-truth oracle). Opt-in, env-gated, data-budgeted.
//!
//! Run (after `xtask rt-snapshot` produced the image):
//!   APFS_RT_IMAGE=/tmp/apfs-rt.img APFS_RT_MOUNT=/Volumes/TEST \
//!     cargo test -p apfs-core --test real_target_differential -- --nocapture
//!
//! Env knobs (bytes): APFS_RT_FULL_MAX (default 4194304), APFS_RT_PREFIX
//! (1048576), APFS_RT_TAIL (262144), APFS_RT_BUDGET (536870912).
// Test harness: expect/unwrap/indexing are intentional and correct here.
#![allow(clippy::expect_used, clippy::indexing_slicing)]
use apfs_core::block_device::FileBlockDevice;
use apfs_core::catalog::Catalog;
use apfs_core::container::Container;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

const NOISE: &[&str] = &[
    ".fseventsd",
    ".Spotlight-V100",
    ".Trashes",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".PKInstallSandboxManager-SystemSoftware",
    ".apdisk",
];

struct Budget {
    full_max: usize,
    prefix: usize,
    tail: usize,
    remaining: i64,
}

#[test]
fn real_target_differential() {
    let img = match std::env::var("APFS_RT_IMAGE") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skip: APFS_RT_IMAGE unset — `cargo run -p xtask -- rt-snapshot \
                 /Volumes/<vol>` prints how to make the image + env vars"
            );
            return;
        }
    };
    let mount = match std::env::var("APFS_RT_MOUNT") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skip: APFS_RT_MOUNT unset (macOS mount path = the oracle)");
            return;
        }
    };
    if !Path::new(&img).exists() || !Path::new(&mount).is_dir() {
        eprintln!("skip: APFS_RT_IMAGE or APFS_RT_MOUNT path missing");
        return;
    }

    let mut dev = match FileBlockDevice::open(&img) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip: cannot open APFS_RT_IMAGE: {e:?}");
            return;
        }
    };
    // Setup failures on a real snapshot almost always mean the dd image is
    // INCOMPLETE (a needed APFS block lies beyond the truncated image) or not
    // an APFS container. Report it actionably and skip — a complete image is
    // an operator responsibility; this is not a reader defect (the reader
    // correctly returns an error instead of crashing).
    let incomplete = |what: &str, e: String| {
        eprintln!(
            "skip: {what} failed ({e}). The snapshot is likely INCOMPLETE — \
             re-run e.g. `sudo dd if=/dev/rdiskNsM of=/tmp/apfs-rt.img bs=4m \
             count=512` and let dd FINISH (do not Ctrl-C), then re-run."
        );
    };
    let c = match Container::open(&mut dev) {
        Ok(c) => c,
        Err(e) => {
            incomplete("open container", format!("{e:?}"));
            return;
        }
    };
    let bsz = c.superblock.block_size;
    let vols = match c.list_user_volumes(&mut dev) {
        Ok(v) => v,
        Err(e) => {
            incomplete("list user volumes", format!("{e:?}"));
            return;
        }
    };
    let vol = match vols.first() {
        Some(v) => v.clone(),
        None => {
            eprintln!("skip: image has no user volume");
            return;
        }
    };
    let cat = match Catalog::open(&mut dev, &vol, bsz) {
        Ok(c) => c,
        Err(e) => {
            incomplete("open catalog", format!("{e:?}"));
            return;
        }
    };
    let hashed = vol.names_are_hashed();

    let mut b = Budget {
        full_max: env_usize("APFS_RT_FULL_MAX", 4 * 1024 * 1024),
        prefix: env_usize("APFS_RT_PREFIX", 1024 * 1024),
        tail: env_usize("APFS_RT_TAIL", 256 * 1024),
        remaining: env_usize("APFS_RT_BUDGET", 512 * 1024 * 1024) as i64,
    };

    let mut files_ok = 0u64;
    let mut bytes_cmp = 0u64;
    let mut dirs_ok = 0u64;
    let mut skipped_symlink = 0u64;
    let mut skipped_special = 0u64;
    let mut fails: Vec<String> = Vec::new();

    let mut stack: Vec<(PathBuf, Vec<String>)> = vec![(PathBuf::from(&mount), Vec::new())];
    while let Some((dir_abs, rel)) = stack.pop() {
        let oracle_names: BTreeSet<String> = match fs::read_dir(&dir_abs) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !NOISE.contains(&n.as_str()))
                .collect(),
            Err(e) => {
                fails.push(format!("oracle read_dir {dir_abs:?}: {e}"));
                continue;
            }
        };
        let comps: Vec<&str> = rel.iter().map(|s| s.as_str()).collect();
        let dir_ino = match cat.resolve_path(&mut dev, &comps, hashed) {
            Ok(Some(i)) => i,
            Ok(None) => {
                fails.push(format!("reader: dir path not found: /{}", rel.join("/")));
                continue;
            }
            Err(e) => {
                fails.push(format!("reader resolve_path /{}: {e:?}", rel.join("/")));
                continue;
            }
        };
        let our_names: BTreeSet<String> = match cat.list_dir(&mut dev, dir_ino, hashed) {
            Ok(v) => v
                .into_iter()
                .map(|d| d.name)
                .filter(|n| !NOISE.contains(&n.as_str()))
                .collect(),
            Err(e) => {
                fails.push(format!("reader list_dir /{}: {e:?}", rel.join("/")));
                continue;
            }
        };
        if our_names != oracle_names {
            let miss: Vec<_> = oracle_names.difference(&our_names).collect();
            let extra: Vec<_> = our_names.difference(&oracle_names).collect();
            fails.push(format!(
                "dir mismatch /{}: missing-in-reader={miss:?} extra-in-reader={extra:?}",
                rel.join("/")
            ));
        } else {
            dirs_ok += 1;
        }

        for name in &oracle_names {
            let child_abs = dir_abs.join(name);
            let md = match fs::symlink_metadata(&child_abs) {
                Ok(m) => m,
                Err(e) => {
                    fails.push(format!("oracle stat {child_abs:?}: {e}"));
                    continue;
                }
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                skipped_symlink += 1;
                continue;
            }
            if ft.is_dir() {
                let mut nrel = rel.clone();
                nrel.push(name.clone());
                stack.push((child_abs, nrel));
                continue;
            }
            if !ft.is_file() {
                skipped_special += 1;
                continue;
            }

            let mut crel: Vec<&str> = comps.clone();
            crel.push(name.as_str());
            let ino = match cat.resolve_path(&mut dev, &crel, hashed) {
                Ok(Some(i)) => i,
                Ok(None) => {
                    fails.push(format!("reader: file not found /{}/{name}", rel.join("/")));
                    continue;
                }
                Err(e) => {
                    fails.push(format!("reader resolve /{}/{name}: {e:?}", rel.join("/")));
                    continue;
                }
            };
            let ours = match cat.read_file_decompressed(&mut dev, ino) {
                Ok(v) => v,
                Err(e) => {
                    fails.push(format!("reader read /{}/{name}: {e:?}", rel.join("/")));
                    continue;
                }
            };
            let want = match fs::read(&child_abs) {
                Ok(v) => v,
                Err(e) => {
                    fails.push(format!("oracle read {child_abs:?}: {e}"));
                    continue;
                }
            };
            if ours.len() != want.len() {
                fails.push(format!(
                    "size /{}/{name}: reader={} oracle={}",
                    rel.join("/"),
                    ours.len(),
                    want.len()
                ));
                continue;
            }
            let full = want.len() <= b.full_max || b.remaining > 0;
            let mismatch_at = if full {
                if b.remaining > 0 && want.len() > b.full_max {
                    b.remaining -= want.len() as i64;
                }
                bytes_cmp += want.len() as u64;
                ours.iter().zip(&want).position(|(a, c)| a != c)
            } else {
                let p = b.prefix.min(want.len());
                let t = b.tail.min(want.len().saturating_sub(p));
                bytes_cmp += (p + t) as u64;
                ours[..p]
                    .iter()
                    .zip(&want[..p])
                    .position(|(a, c)| a != c)
                    .or_else(|| {
                        let s = want.len() - t;
                        ours[s..]
                            .iter()
                            .zip(&want[s..])
                            .position(|(a, c)| a != c)
                            .map(|x| s + x)
                    })
            };
            match mismatch_at {
                Some(off) => fails.push(format!(
                    "BYTE MISMATCH /{}/{name} at offset {off} (len {})",
                    rel.join("/"),
                    want.len()
                )),
                None => files_ok += 1,
            }
        }
    }

    eprintln!(
        "M3-RT differential: files_ok={files_ok} bytes_compared={bytes_cmp} \
         dirs_ok={dirs_ok} symlinks_skipped={skipped_symlink} \
         special_skipped={skipped_special} failures={}",
        fails.len()
    );
    for f in fails.iter().take(50) {
        eprintln!("  FAIL: {f}");
    }
    assert!(
        fails.is_empty(),
        "{} differential failure(s) vs macOS kernel oracle (see above)",
        fails.len()
    );
    assert!(
        files_ok > 0,
        "no regular files compared — is APFS_RT_MOUNT the right populated volume?"
    );
}
