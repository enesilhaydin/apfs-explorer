//! Every decmpfs method macOS reads, read by us.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, python3, fsck_apfs).
//!
//! Two sources of truth. `scripts/decmpfs-samples.py author` stores files
//! with each of the ten methods the way macOS tools do (zlib, LZVN, plain,
//! LZFSE and LZBITMAP, inline and in the resource fork, compressible and
//! incompressible content, one chunk and several) and has macOS read every
//! one back before the image is handed to us, so a layout macOS would not
//! accept never reaches an assertion here. `harvest` copies the attributes
//! of files macOS itself compressed on this machine, which is where the
//! methods Apple's own tools pick come from. A reader that knows only some
//! of the methods shows the right size for such a file and then fails to
//! open it.
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
mod decmpfs_methods {
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use std::collections::BTreeMap;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-decmpfs-methods.img";
    const PLAIN: &str = "/tmp/apfs-decmpfs-methods-plain";
    const HARVEST: &str = "/tmp/apfs-decmpfs-harvest";
    const SCRIPT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/decmpfs-samples.py"
    );
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        assert_eq!(&hdr[..8], b"EFI PART");
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match raw.get(off..off + sz) {
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

    /// Attach the image; returns (mount point, whole-disk node).
    fn attach(mount_it: bool) -> (String, String) {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if !mount_it {
            args.push("-nomount");
        }
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        (mount, parent)
    }

    fn detach(parent: &str) {
        for attempt in 0..8 {
            let st = Command::new("hdiutil")
                .args(["detach", parent])
                .output()
                .expect("hdiutil detach");
            if st.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let st = Command::new("hdiutil")
            .args(["detach", "-force", parent])
            .status()
            .expect("hdiutil detach -force");
        assert!(st.success(), "hdiutil detach failed");
    }

    /// Strict: any inconsistency fsck_apfs mentions counts, not just its exit code.
    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&parent);
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

    struct Authored {
        name: String,
        method: u32,
        size: usize,
        /// The attribute names macOS lists for an ordinary caller.
        xattrs: Vec<String>,
    }

    fn name_list(field: &str) -> Vec<String> {
        if field == "-" {
            return Vec::new();
        }
        field.split(',').map(str::to_string).collect()
    }

    /// A file whose decmpfs state is inconsistent, with what macOS made of it:
    /// the size it reported (or the errno) and the length it read (or the errno).
    struct Oddity {
        name: String,
        setup: String,
        stat: Result<u64, i32>,
        read: Result<usize, i32>,
        xattrs: Vec<String>,
    }

    fn outcome<T: std::str::FromStr>(field: &str) -> Option<Result<T, i32>> {
        let (_, rest) = field.split_once('=')?;
        let (kind, value) = rest.split_once(':')?;
        match kind {
            "OK" => value.parse().ok().map(Ok),
            "ERR" => value.parse().ok().map(Err),
            _ => None,
        }
    }

    /// Build an image and let the authoring script fill it while macOS has it
    /// mounted. With `oddities`, the inconsistent files are added too; one of
    /// them (a resource-fork method with no fork) is corrupt by fsck's rules,
    /// so that image is not fsck-checked.
    fn build_image(oddities: bool) -> (Vec<Authored>, Vec<Oddity>) {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-decmpfs-methods.dmg");
        let _ = std::fs::remove_dir_all(PLAIN);
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "DECMPM",
                "/tmp/apfs-decmpfs-methods",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-decmpfs-methods.dmg", IMG).expect("rename");

        let (mount, parent) = attach(true);
        assert!(!mount.is_empty(), "the fresh image must mount");
        let mut args = vec![SCRIPT, "author", &mount, PLAIN];
        if oddities {
            args.push("--oddities");
        }
        let out = Command::new("python3")
            .args(&args)
            .output()
            .expect("python3");
        let _ = Command::new("sync").status();
        detach(&parent);
        assert!(
            out.status.success(),
            "authoring script failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let authored = stdout
            .lines()
            .filter(|l| !l.starts_with("odd "))
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some(Authored {
                    name: it.next()?.to_string(),
                    method: it.next()?.parse().ok()?,
                    size: it.next()?.parse().ok()?,
                    xattrs: name_list(it.next()?),
                })
            })
            .collect();
        let oddities = stdout
            .lines()
            .filter_map(|l| l.strip_prefix("odd "))
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some(Oddity {
                    name: it.next()?.to_string(),
                    setup: it.next()?.to_string(),
                    stat: outcome(it.next()?)?,
                    read: outcome(it.next()?)?,
                    xattrs: name_list(it.next()?.strip_prefix("xattrs=")?),
                })
            })
            .collect();
        (authored, oddities)
    }

    /// The attributes we list, and hand out by name, must be exactly the ones
    /// macOS shows: a compressed file's com.apple.decmpfs and
    /// com.apple.ResourceFork are storage, not attributes, and stay hidden;
    /// on a file that is not compressed they are ordinary attributes.
    fn check_visible_xattrs<D: apfs_core::block_device::BlockDevice>(
        fs: &mut apfs::FsView<D>,
        path: &str,
        want: &[String],
        failures: &mut Vec<String>,
    ) {
        let mut got = match fs.list_xattrs(path) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{path}: list_xattrs failed: {e:?}"));
                return;
            }
        };
        got.sort();
        if got != want {
            failures.push(format!(
                "{path}: macOS lists attributes {want:?}, we list {got:?}"
            ));
        }
        for name in ["com.apple.decmpfs", "com.apple.ResourceFork"] {
            let visible = want.iter().any(|n| n == name);
            match fs.get_xattr(path, name) {
                Ok(Some(_)) if !visible => failures.push(format!(
                    "{path}: {name} is hidden on macOS but we hand it out"
                )),
                Ok(None) if visible => failures.push(format!(
                    "{path}: {name} is visible on macOS but we do not hand it out"
                )),
                Err(e) => failures.push(format!("{path}: get_xattr {name} failed: {e:?}")),
                _ => {}
            }
        }
    }

    /// Reads of parts of the file (inside a chunk, across a chunk boundary,
    /// past the end) must give exactly the corresponding bytes.
    fn check_ranges<D: apfs_core::block_device::BlockDevice>(
        fs: &mut apfs::FsView<D>,
        path: &str,
        want: &[u8],
        failures: &mut Vec<String>,
    ) {
        let size = want.len();
        let probes = [
            (1usize, 100usize),
            (size / 2, 4097),
            (65_530, 20),
            (size.saturating_sub(3), 10),
            (size, 10),
        ];
        for (off, n) in probes {
            let expect = if off >= size {
                &[][..]
            } else {
                &want[off..(off + n).min(size)]
            };
            match fs.read(path, off as u64, n) {
                Ok(got) if got == expect => {}
                Ok(got) => failures.push(format!(
                    "{path}: read of {n} bytes at {off} gave {} bytes that differ",
                    got.len()
                )),
                Err(e) => {
                    failures.push(format!("{path}: read of {n} bytes at {off} failed: {e:?}"))
                }
            }
        }
    }

    fn summarize(label: &str, per_method: &BTreeMap<u32, (usize, usize)>) {
        for (m, (ok, all)) in per_method {
            println!("{label}: method {m}: {ok}/{all} ok");
        }
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn every_method_macos_accepts_reads_back_through_us() {
        if !gate() {
            return;
        }
        let (files, _) = build_image(false);
        assert!(
            files.len() >= 40,
            "the script must author every method and shape"
        );
        run_fsck("authored image");

        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = apfs::FsView::open(dev).expect("FsView::open");

        let mut failures = Vec::new();
        let mut per_method: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
        for f in &files {
            let want = std::fs::read(format!("{PLAIN}/{}", f.name)).expect("plain copy");
            assert_eq!(want.len(), f.size, "{}: manifest size", f.name);
            let path = format!("/{}", f.name);
            let entry = per_method.entry(f.method).or_default();
            entry.1 += 1;
            match fs.getattr(&path) {
                Ok(Some(a)) if a.size as usize == f.size => {}
                other => {
                    failures.push(format!(
                        "{}: getattr gave {other:?}, want size {}",
                        f.name, f.size
                    ));
                    continue;
                }
            }
            match fs.read(&path, 0, f.size) {
                Ok(got) if got == want => entry.0 += 1,
                Ok(got) => failures.push(format!(
                    "{}: content differs ({} bytes back, first difference at {:?})",
                    f.name,
                    got.len(),
                    got.iter().zip(want.iter()).position(|(a, b)| a != b)
                )),
                Err(e) => failures.push(format!("{}: read failed: {e:?}", f.name)),
            }
            check_visible_xattrs(&mut fs, &path, &f.xattrs, &mut failures);
            check_ranges(&mut fs, &path, &want, &mut failures);
        }
        summarize("authored", &per_method);
        assert!(
            failures.is_empty(),
            "{} of {} files macOS reads back are not read by us:\n{}",
            failures.len(),
            files.len(),
            failures.join("\n")
        );
    }

    /// Files whose decmpfs state is inconsistent (attribute without the flag,
    /// flag with a non-empty data fork, unknown method, missing fork, ...) must
    /// come out of our reader exactly as they come out of macOS: the same size
    /// and content where macOS serves them, an error where macOS fails.
    #[test]
    #[ignore = "scratch-image gated"]
    fn inconsistent_state_reads_the_way_macos_reads_it() {
        if !gate() {
            return;
        }
        let (_, oddities) = build_image(true);
        assert!(oddities.len() >= 6, "the script must set up every oddity");

        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = apfs::FsView::open(dev).expect("FsView::open");

        let mut failures = Vec::new();
        for o in &oddities {
            let path = format!("/{}", o.name);
            println!(
                "oddity {} ({}): macOS stat {:?} read {:?}",
                o.name, o.setup, o.stat, o.read
            );
            match (o.stat, fs.getattr(&path)) {
                (Ok(size), Ok(Some(a))) if a.size == size => {}
                (Err(_), Err(_)) => {}
                (want, got) => failures.push(format!(
                    "{}: macOS stat says {want:?}, our getattr gave {got:?}",
                    o.name
                )),
            }
            match (o.read, fs.read(&path, 0, 1 << 20)) {
                (Ok(n), Ok(got)) => {
                    let want = std::fs::read(format!("{PLAIN}/{}", o.name)).expect("plain copy");
                    if got.len() != n || got != want {
                        failures.push(format!(
                            "{}: macOS reads {n} bytes, we read {} and they differ",
                            o.name,
                            got.len()
                        ));
                    }
                }
                (Err(_), Err(_)) => {}
                (want, got) => failures.push(format!(
                    "{}: macOS read says {want:?}, our read gave {:?}",
                    o.name,
                    got.map(|v| v.len())
                )),
            }
            check_visible_xattrs(&mut fs, &path, &o.xattrs, &mut failures);
        }
        assert!(
            failures.is_empty(),
            "{} oddities are not read the way macOS reads them:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn files_apple_itself_compressed_decode_from_their_attributes() {
        if !gate() {
            return;
        }
        let _ = std::fs::remove_dir_all(HARVEST);
        let out = Command::new("python3")
            .args([
                SCRIPT,
                "harvest",
                HARVEST,
                "--per-method",
                "120",
                "--max-size",
                "8388608",
            ])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "harvest failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let index: Vec<(usize, u32, usize, String)> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.splitn(4, ' ');
                Some((
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.to_string(),
                ))
            })
            .collect();
        assert!(
            !index.is_empty(),
            "macOS keeps compressed files under /usr and /System"
        );

        let mut failures = Vec::new();
        let mut per_method: BTreeMap<u32, (usize, usize)> = BTreeMap::new();
        for (n, method, size, path) in &index {
            let attr = std::fs::read(format!("{HARVEST}/{n}.decmpfs")).expect("decmpfs copy");
            let fork = std::fs::read(format!("{HARVEST}/{n}.rsrc")).ok();
            let plain = std::fs::read(format!("{HARVEST}/{n}.plain")).expect("plain copy");
            assert_eq!(plain.len(), *size, "{path}: index size");
            let entry = per_method.entry(*method).or_default();
            entry.1 += 1;
            match apfs_core::decmpfs_decompress(&attr, fork.as_deref()) {
                Ok(got) if got == plain => entry.0 += 1,
                Ok(got) => failures.push(format!(
                    "{path} (method {method}): content differs ({} bytes back)",
                    got.len()
                )),
                Err(e) => failures.push(format!("{path} (method {method}): {e:?}")),
            }
        }
        summarize("harvested", &per_method);
        assert!(
            failures.is_empty(),
            "{} of {} files macOS compressed itself do not decode:\n{}",
            failures.len(),
            index.len(),
            failures.join("\n")
        );
    }
}
