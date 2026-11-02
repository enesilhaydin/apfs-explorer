//! The records an attribute stream is made of, ours against macOS's.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! An attribute too large to sit in its record gets a stream of its own,
//! keyed by an id that is neither the file's nor its content stream's. The
//! reference implementations never write a data-stream-id record for such a
//! stream, since an attribute stream is never shared; the checker in
//! apfsprogs reports one ("xattrs can't be cloned"). This lets macOS write a
//! large attribute, lists the data-stream-id records it leaves, has our
//! writer do the same on a second file, and expects the same shape.
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
mod xattr_stream_records {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{set_xattr, write_file};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-xattr-stream-records.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const NAME: &str = "user.large";
    /// Past the embedded limit, so the value gets a stream.
    const VALUE_LEN: usize = 10_000;

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
    fn in_txn(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8])) {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
    }
    fn open_fs() -> apfs::FsView<Dev> {
        apfs::FsView::open(open_dev()).expect("FsView::open")
    }
    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string()
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
    /// macOS mounts the image read-write and runs `f` on the mount point.
    fn with_macos_mount<T>(f: impl FnOnce(&str) -> T) -> T {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
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
        let out = f(&mount);
        let _ = Command::new("sync").status();
        detach(&disk);
        out
    }
    /// `setxattr` through libc; the value comes from a file.
    fn macos_set_xattr(path: &str, name: &str, value: &[u8]) -> bool {
        let tmp = "/tmp/apfs-xattr-stream-records.value";
        std::fs::write(tmp, value).unwrap();
        let script = "import ctypes, sys\n\
            libc = ctypes.CDLL(None, use_errno=True)\n\
            libc.setxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]\n\
            v = open(sys.argv[3], 'rb').read()\n\
            sys.exit(0 if libc.setxattr(sys.argv[1].encode(), sys.argv[2].encode(), v, len(v), 0, 0) == 0 else 1)\n";
        Command::new("python3")
            .args(["-c", script, path, name, tmp])
            .status()
            .expect("python3")
            .success()
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
    fn our_attribute_stream_leaves_the_records_macos_leaves() {
        if !gate() {
            return;
        }
        gen_scratch();
        for name in ["by-macos.txt", "by-us.txt"] {
            in_txn(|t, v, o| {
                write_file(t, v, o, 2, name, b"content").expect("write");
            });
        }
        let value: Vec<u8> = (0..VALUE_LEN).map(|i| (i % 241) as u8).collect();
        let before = open_fs().dstream_ids().expect("dstream ids");

        with_macos_mount(|mount| {
            assert!(
                macos_set_xattr(&format!("{mount}/by-macos.txt"), NAME, &value),
                "macOS must take the attribute"
            );
        });
        let after_macos = open_fs().dstream_ids().expect("dstream ids");
        let macos_added: Vec<(u64, u32)> = after_macos
            .iter()
            .filter(|r| !before.contains(r))
            .copied()
            .collect();
        println!(
            "xattr-stream-records: macOS's {VALUE_LEN}-byte attribute added data-stream-id \
             records {macos_added:?} (had {before:?})"
        );

        let ino = open_fs()
            .lookup("/by-us.txt")
            .expect("lookup")
            .expect("by-us.txt");
        in_txn(|t, v, o| {
            set_xattr(t, v, o, ino, NAME, &value).expect("set attribute");
        });
        let after_ours = open_fs().dstream_ids().expect("dstream ids");
        let ours_added: Vec<(u64, u32)> = after_ours
            .iter()
            .filter(|r| !after_macos.contains(r))
            .copied()
            .collect();
        println!("xattr-stream-records: our attribute added data-stream-id records {ours_added:?}");
        assert_eq!(
            ours_added.len(),
            macos_added.len(),
            "our attribute stream must leave the same number of data-stream-id records \
             macOS's leaves ({ours_added:?} vs {macos_added:?})"
        );
        let mut fs = open_fs();
        for name in ["by-macos.txt", "by-us.txt"] {
            let got = fs
                .get_xattr(&format!("/{name}"), NAME)
                .expect("get")
                .expect("present");
            assert!(got == value, "{name}: the attribute must read back whole");
        }
        drop(fs);
        run_fsck("after both attributes");
        let _ = std::fs::remove_file(IMG);
    }
}
