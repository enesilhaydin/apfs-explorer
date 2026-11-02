//! Names whose case fold is not their lowercase, judged by macOS.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! On a case-insensitive volume a directory record is keyed by the hash of
//! the name's normalized, case-folded form, and the fold macOS applies is the
//! full Unicode one: `straße` folds to `strasse`, `ﬁle` to `file`, a final
//! sigma to the ordinary one. A writer that lowercases instead stores a hash
//! macOS never computes, so the file cannot be found by name there and the
//! checker reports the record. The same fold decides whether a new name
//! collides with an existing one.
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
mod name_hash_macos {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, rename, write_file};
    use apfs_write::txn::{Transaction, TxnError};
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-name-hash.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    /// `(name we create, other spellings macOS must resolve to it)`.
    const NAMES: &[(&str, &[&str])] = &[
        ("straße.txt", &["STRASSE.txt", "Strasse.txt", "STRAßE.txt"]),
        ("ﬁle.txt", &["file.txt", "FILE.txt"]),
        ("οδυσσευς.txt", &["ΟΔΥΣΣΕΥΣ.txt", "οδυσσευσ.txt"]),
        ("café.txt", &["cafe\u{301}.txt", "CAFÉ.txt"]),
        ("İ-dotted.txt", &["i\u{307}-dotted.txt"]),
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
        let _ = std::fs::remove_file(IMG);
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                IMAGE_MB,
            ])
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn find_apfs_range() -> (u64, u64) {
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut head = vec![0u8; 64 * 1024];
        f.read_exact(&mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match head.get(off..off + sz) {
                Some(e) => e,
                None => break,
            };
            if e.get(..16) == Some(&APFS_GPT_TYPE[..]) {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                return (s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
    }
    fn open_dev() -> Dev {
        let (base, len) = find_apfs_range();
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        WritableOffsetBlockDevice::new(inner, base, len)
    }
    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let mut dev = open_dev();
        let c = Container::open(&mut dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(&mut dev, fs_oid, nx.obj.xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }
    fn in_txn<T>(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> T) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        let out = f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
        out
    }

    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node")
    }
    fn detach(disk: &str) {
        for attempt in 0..8u64 {
            let st = Command::new("hdiutil")
                .args(["detach", disk])
                .output()
                .expect("detach");
            if st.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }
    fn run_fsck(label: &str) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-nomount",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let part = attached
            .lines()
            .filter(|l| l.contains("Apple_APFS"))
            .filter_map(|l| l.split_whitespace().next())
            .next()
            .expect("no APFS partition node")
            .to_string();
        let rdev = part.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&disk);
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "{label}: fsck_apfs reported corruption:\n{report}"
        );
        let complaints: Vec<&str> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("error:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .collect();
        assert!(
            complaints.is_empty(),
            "{label}: fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn names_with_a_full_case_fold_are_found_by_macos_and_collide_with_their_folds() {
        if !gate() {
            return;
        }
        gen_scratch();
        for (name, _) in NAMES {
            in_txn(|t, v, o| {
                write_file(t, v, o, 2, name, name.as_bytes())
                    .unwrap_or_else(|e| panic!("write {name}: {e}"));
            });
        }
        run_fsck("after creating the names");

        // A spelling that folds to an existing name is that name: creating it
        // has to be refused, not stored beside the original.
        for (name, others) in NAMES {
            for other in *others {
                let outcome = in_txn(|t, v, o| create_file(t, v, o, 2, other));
                assert!(
                    matches!(outcome, Err(TxnError::AlreadyExists(_))),
                    "{other:?} folds to {name:?} and must be refused, got {outcome:?}"
                );
            }
        }
        run_fsck("after the refused duplicates");

        // A rename onto a spelling of an existing name is a collision too, and
        // with the replace flag it replaces that file rather than sitting
        // beside it. The renamed file takes the name it was given.
        in_txn(|t, v, o| {
            write_file(t, v, o, 2, "other.txt", b"other").expect("write other");
        });
        let outcome = in_txn(|t, v, o| rename(t, v, o, 2, "other.txt", "STRAßE.txt", false));
        assert!(
            matches!(outcome, Err(TxnError::AlreadyExists(_))),
            "renaming onto a spelling of straße.txt must be refused, got {outcome:?}"
        );
        in_txn(|t, v, o| {
            rename(t, v, o, 2, "other.txt", "STRAßE.txt", true).expect("rename with replace");
        });
        run_fsck("after the replacing rename");

        // macOS resolves every spelling to the file we created.
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-readonly",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach (mount) failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let mount = attached
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let mut failures = Vec::new();
        for (name, others) in NAMES {
            // The replacing rename above gave the straße entry other.txt's
            // content under the spelling STRAßE.txt.
            let expected: &[u8] = if *name == "straße.txt" {
                b"other"
            } else {
                name.as_bytes()
            };
            for spelling in std::iter::once(name).chain(others.iter()) {
                match std::fs::read(format!("{mount}/{spelling}")) {
                    Ok(got) if got == expected => {}
                    Ok(got) => failures.push(format!(
                        "{spelling:?}: macOS returned {:?}",
                        String::from_utf8_lossy(&got)
                    )),
                    Err(e) => failures.push(format!("{spelling:?}: {e}")),
                }
            }
        }
        let listed = std::fs::read_dir(&mount)
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| !n.starts_with('.'))
            .count();
        detach(&disk);
        assert!(
            failures.is_empty(),
            "macOS could not resolve:\n{}",
            failures.join("\n")
        );
        assert_eq!(
            listed,
            NAMES.len(),
            "macOS must list exactly the names created"
        );
        let _ = std::fs::remove_file(IMG);
    }
}
