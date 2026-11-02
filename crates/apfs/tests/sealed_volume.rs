//! A sealed volume read by us against the same volume read by macOS.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS).
//!
//! A sealed (signed) volume keeps its file extents in a tree of their own
//! instead of the catalog. macOS ships such volumes as cryptex images under
//! the Preboot volume; one of them is copied to a scratch path, macOS mounts
//! the copy read-only, and every directory listing, symlink target and file
//! byte we read must equal what macOS reads.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
    )
)]

#[cfg(test)]
mod sealed_volume {
    use apfs_core::block_device::FileBlockDevice;
    use std::collections::BTreeSet;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-sealed-volume.img";
    const PREBOOT: &str = "/System/Volumes/Preboot";
    /// Directory entry type of a directory, a regular file and a symlink
    /// (low four bits of the record's flags).
    const DT_DIR: u16 = 4;
    const DT_REG: u16 = 8;
    const DT_LNK: u16 = 10;
    const DT_MASK: u16 = 0xF;
    /// Bytes read from our view per call.
    const CHUNK: usize = 1 << 20;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// The image named by `APFS_SEALED_IMAGE`, or else the smallest cryptex
    /// image this Mac carries, if any.
    fn find_cryptex() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("APFS_SEALED_IMAGE") {
            return Some(p.into());
        }
        let mut found = Vec::new();
        for vol in std::fs::read_dir(PREBOOT).ok()?.flatten() {
            let dir = vol.path().join("cryptex1/current");
            for name in ["app.dmg", "os.dmg"] {
                let p = dir.join(name);
                if let Ok(m) = std::fs::metadata(&p) {
                    found.push((m.len(), p));
                }
            }
        }
        found.sort();
        found.into_iter().next().map(|(_, p)| p)
    }

    fn attach_read_only() -> (String, String) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-readonly",
                "-nobrowse",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let disk = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        (mount, disk)
    }

    fn detach(disk: &str) {
        let _ = Command::new("hdiutil").args(["detach", disk]).status();
    }

    #[derive(Default)]
    struct Tally {
        dirs: usize,
        files: usize,
        links: usize,
        bytes: u64,
        mismatches: Vec<String>,
    }

    fn walk(fs: &mut apfs::FsView<FileBlockDevice>, mount: &str, rel: &str, tally: &mut Tally) {
        tally.dirs += 1;
        let ours = fs
            .read_dir(if rel.is_empty() { "/" } else { rel })
            .expect("read_dir");
        let our_names: BTreeSet<String> = ours.iter().map(|e| e.name.clone()).collect();
        let theirs: BTreeSet<String> = std::fs::read_dir(format!("{mount}{rel}"))
            .expect("macOS read_dir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        if our_names != theirs {
            tally.mismatches.push(format!("{rel}/: listing differs"));
        }
        for e in ours {
            let path = format!("{rel}/{}", e.name);
            match e.flags & DT_MASK {
                DT_DIR => walk(fs, mount, &path, tally),
                DT_LNK => {
                    tally.links += 1;
                    let ours = fs.read_symlink(&path).expect("read_symlink");
                    let theirs = std::fs::read_link(format!("{mount}{path}"))
                        .map(|t| t.to_string_lossy().into_owned().into_bytes())
                        .ok();
                    if ours != theirs {
                        tally
                            .mismatches
                            .push(format!("{path}: symlink target differs"));
                    }
                }
                DT_REG => {
                    tally.files += 1;
                    let theirs = std::fs::read(format!("{mount}{path}")).expect("macOS read");
                    let mut ours = Vec::with_capacity(theirs.len());
                    loop {
                        let got = fs.read(&path, ours.len() as u64, CHUNK).expect("read");
                        if got.is_empty() {
                            break;
                        }
                        ours.extend_from_slice(&got);
                    }
                    tally.bytes += theirs.len() as u64;
                    if ours != theirs {
                        tally.mismatches.push(format!(
                            "{path}: content differs ({} bytes ours, {} macOS)",
                            ours.len(),
                            theirs.len()
                        ));
                    }
                }
                other => tally
                    .mismatches
                    .push(format!("{path}: unexpected entry type {other}")),
            }
        }
    }

    #[test]
    #[ignore = "macOS-gated; reads a cryptex image from the Preboot volume"]
    fn a_sealed_volume_reads_the_way_macos_reads_it() {
        if !gate() {
            return;
        }
        let Some(src) = find_cryptex() else {
            println!("sealed-volume: no cryptex image on this Mac - skip");
            return;
        };
        std::fs::copy(&src, IMG).expect("copy the cryptex image");
        let (mount, disk) = attach_read_only();
        let mut fs =
            apfs::FsView::open(FileBlockDevice::open(IMG).expect("open")).expect("FsView::open");
        let mut tally = Tally::default();
        walk(&mut fs, &mount, "", &mut tally);
        drop(fs);
        detach(&disk);
        let _ = std::fs::remove_file(IMG);
        println!(
            "sealed-volume: {} dirs, {} files ({} bytes), {} symlinks, {} mismatches",
            tally.dirs,
            tally.files,
            tally.bytes,
            tally.links,
            tally.mismatches.len()
        );
        assert!(tally.files > 0, "the walk must reach files");
        assert!(
            tally.mismatches.is_empty(),
            "every entry must read the way macOS reads it; first ones:\n{}",
            tally
                .mismatches
                .iter()
                .take(15)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
