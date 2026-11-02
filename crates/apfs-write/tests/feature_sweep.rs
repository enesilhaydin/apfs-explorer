//! Every write operation against every kind of file macOS can hand us.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, ditto, cp -c, ln,
//! truncate, xattr, fsck_apfs).
//!
//! macOS authors the starting volume: plain files, a compressed one, a clone
//! pair sharing a stream, a hard-link pair, a sparse file grown without
//! writing, a file with a small and a large attribute, a symbolic link and a
//! nested directory. Every operation the writer offers is then applied to
//! them in turn - reading, replacing, growing, cutting, attributes, renaming,
//! moving, linking, unlinking, cloning, symbolic links, directories,
//! snapshots and a revert - with three arbiters at every step: the volume is
//! fsck-clean (warnings count), our reader agrees with an in-memory model of
//! what every path should hold, and at the end macOS mounts the result and
//! reads every surviving path itself.
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
mod feature_sweep {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        clone_file, create_symlink, delete_xattr, link, mkdir, move_entry, overwrite_existing_file,
        rename, set_xattr, truncate_file_fast, unlink, write_file, TruncateOutcome,
    };
    use apfs_write::snapshot::{create_snapshot, delete_snapshot, revert_to_snapshot};
    use apfs_write::txn::Transaction;
    use std::collections::{BTreeMap, BTreeSet};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-feature-sweep.img";
    const CMP_PLAIN: &str = "/tmp/apfs-feature-sweep-cmp-plain";
    const SAMPLES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/decmpfs-samples.py"
    );
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT: u64 = 2;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u64 * 7 + seed * 131) % 251) as u8)
            .collect()
    }

    // ------------------------------------------------------------------
    // The model: what every path should hold. Hard links share a node.
    // ------------------------------------------------------------------
    #[derive(Clone, Default)]
    struct Model {
        nodes: Vec<Vec<u8>>,
        files: BTreeMap<String, usize>,
        dirs: BTreeSet<String>,
        symlinks: BTreeMap<String, Vec<u8>>,
        xattrs: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    }

    impl Model {
        fn add_file(&mut self, path: &str, content: Vec<u8>) {
            self.nodes.push(content);
            self.files.insert(path.to_string(), self.nodes.len() - 1);
        }
        fn content(&self, path: &str) -> &[u8] {
            &self.nodes[self.files[path]]
        }
        fn set_content(&mut self, path: &str, content: Vec<u8>) {
            let n = self.files[path];
            self.nodes[n] = content;
        }
        fn link(&mut self, from: &str, to: &str) {
            let n = self.files[from];
            self.files.insert(to.to_string(), n);
        }
        fn rename(&mut self, from: &str, to: &str) {
            if let Some(n) = self.files.remove(from) {
                self.files.insert(to.to_string(), n);
            }
            if let Some(x) = self.xattrs.remove(from) {
                self.xattrs.insert(to.to_string(), x);
            }
        }
        fn remove(&mut self, path: &str) {
            self.files.remove(path);
            self.symlinks.remove(path);
            self.dirs.remove(path);
            self.xattrs.remove(path);
        }
        fn parent_and_name(path: &str) -> (String, String) {
            let (dir, name) = path.rsplit_once('/').unwrap();
            let dir = if dir.is_empty() {
                "/".to_string()
            } else {
                dir.to_string()
            };
            (dir, name.to_string())
        }
    }

    // ------------------------------------------------------------------
    // Image built by macOS.
    // ------------------------------------------------------------------
    fn build_image(model: &mut Model) {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-feature-sweep.dmg");
        // `APFS_SWEEP_FS` picks the volume flavour macOS formats: `APFS` (the
        // default, case-insensitive) or `Case-sensitive APFS`.
        let fs_kind = std::env::var("APFS_SWEEP_FS").unwrap_or_else(|_| "APFS".to_string());
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "128m",
                "-layout",
                "GPTSPUD",
                "-fs",
                &fs_kind,
                "-volname",
                "SWEEP",
                "/tmp/apfs-feature-sweep",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-feature-sweep.dmg", IMG).expect("rename");

        let (m, parent) = attach(true);
        let _guard = Attached(parent.clone());
        assert!(!m.is_empty(), "the fresh image must mount");
        let w = |rel: &str, data: &[u8]| std::fs::write(format!("{m}/{rel}"), data).expect(rel);
        let run = |tool: &str, args: &[&str]| {
            let st = Command::new(tool).args(args).status().expect(tool);
            assert!(st.success(), "{tool} {args:?}");
        };

        std::fs::create_dir_all(format!("{m}/sub/deeper")).unwrap();
        model.dirs.insert("/sub".into());
        model.dirs.insert("/sub/deeper".into());

        let plain = pattern(200 * 1024, 1);
        w("plain.bin", &plain);
        model.add_file("/plain.bin", plain);

        // Compressed: ditto needs a source directory.
        let src = "/tmp/apfs-feature-sweep-src";
        let _ = std::fs::remove_dir_all(src);
        std::fs::create_dir_all(src).unwrap();
        let comp = pattern(180 * 1024, 2);
        std::fs::write(format!("{src}/compressed.bin"), &comp).unwrap();
        run("ditto", &["--hfsCompression", src, &m]);
        let flags = Command::new("stat")
            .args(["-f", "%Sf", &format!("{m}/compressed.bin")])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&flags.stdout).contains("compressed"),
            "macOS must have compressed the sample"
        );
        model.add_file("/compressed.bin", comp);

        // Every compression method macOS reads, stored the way its tools
        // store them (scripts/decmpfs-samples.py), under /cmp.
        std::fs::create_dir_all(format!("{m}/cmp")).unwrap();
        model.dirs.insert("/cmp".into());
        let _ = std::fs::remove_dir_all(CMP_PLAIN);
        let out = Command::new("python3")
            .args([SAMPLES, "author", &format!("{m}/cmp"), CMP_PLAIN])
            .output()
            .expect("python3");
        assert!(
            out.status.success(),
            "decmpfs-samples.py author failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let name = line.split_whitespace().next().unwrap();
            let content = std::fs::read(format!("{CMP_PLAIN}/{name}")).unwrap();
            model.add_file(&format!("/cmp/{name}"), content);
        }

        let clone = pattern(160 * 1024, 3);
        w("clone_a.bin", &clone);
        run(
            "cp",
            &[
                "-c",
                &format!("{m}/clone_a.bin"),
                &format!("{m}/clone_b.bin"),
            ],
        );
        model.add_file("/clone_a.bin", clone.clone());
        model.add_file("/clone_b.bin", clone);

        // A second clone pair, left untouched until something replaces one side.
        let clone2 = pattern(90 * 1024, 9);
        w("clone2_a.bin", &clone2);
        run(
            "cp",
            &[
                "-c",
                &format!("{m}/clone2_a.bin"),
                &format!("{m}/clone2_b.bin"),
            ],
        );
        model.add_file("/clone2_a.bin", clone2.clone());
        model.add_file("/clone2_b.bin", clone2);

        let linked = pattern(60 * 1024, 4);
        w("link_a.txt", &linked);
        run(
            "ln",
            &[&format!("{m}/link_a.txt"), &format!("{m}/sub/link_b.txt")],
        );
        model.add_file("/link_a.txt", linked);
        model.link("/link_a.txt", "/sub/link_b.txt");

        let sparse_head = pattern(10_000, 5);
        w("sparse.bin", &sparse_head);
        run("truncate", &["-s", "100000", &format!("{m}/sparse.bin")]);
        let mut sparse = sparse_head;
        sparse.resize(100_000, 0);
        model.add_file("/sparse.bin", sparse);

        let tagged = pattern(20 * 1024, 6);
        w("tagged.bin", &tagged);
        run(
            "xattr",
            &["-w", "user.small", "hello", &format!("{m}/tagged.bin")],
        );
        let big_attr = pattern(100 * 1024, 7);
        // xattr(1) takes the value on the command line, which a 100 KiB value
        // overflows; the system call itself has no such limit.
        std::fs::write("/tmp/apfs-feature-sweep-attr.bin", &big_attr).unwrap();
        run(
            "python3",
            &[
                "-c",
                "import ctypes, sys\n\
                 libc = ctypes.CDLL(None, use_errno=True)\n\
                 data = open(sys.argv[2], 'rb').read()\n\
                 rc = libc.setxattr(sys.argv[1].encode(), b'user.large', data, len(data), 0, 0)\n\
                 sys.exit(0 if rc == 0 else ctypes.get_errno() or 1)",
                &format!("{m}/tagged.bin"),
                "/tmp/apfs-feature-sweep-attr.bin",
            ],
        );
        model.add_file("/tagged.bin", tagged);
        let mut xs = BTreeMap::new();
        xs.insert("user.small".to_string(), b"hello".to_vec());
        xs.insert("user.large".to_string(), big_attr);
        model.xattrs.insert("/tagged.bin".into(), xs);

        let nested = pattern(30 * 1024, 8);
        w("sub/deeper/nested.bin", &nested);
        model.add_file("/sub/deeper/nested.bin", nested);

        run("ln", &["-s", "plain.bin", &format!("{m}/sym")]);
        model.symlinks.insert("/sym".into(), b"plain.bin".to_vec());

        let _ = Command::new("sync").status();
    }

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
        // A mount point may contain spaces when a volume of the same name is
        // already mounted, so take everything from "/Volumes/" to end of line.
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        (mount, parent)
    }

    fn detach(parent: &str) {
        let _ = Command::new("hdiutil").args(["detach", parent]).status();
    }

    /// Detaches when dropped, so a failing assertion does not leave the image
    /// mounted for the next run to trip over.
    struct Attached(String);
    impl Drop for Attached {
        fn drop(&mut self) {
            detach(&self.0);
        }
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
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

    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
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

    fn open_txn() -> Transaction<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    fn open_fs() -> apfs::FsView<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        apfs::FsView::open(WritableOffsetBlockDevice::new(inner, base, len)).expect("FsView::open")
    }

    /// One operation in one transaction.
    fn with_txn<T>(
        f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, apfs_write::txn::TxnError>,
    ) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = open_txn();
        let r = f(&mut txn, &vsb, &om).expect("operation");
        txn.commit().expect("commit");
        r
    }

    fn ino(path: &str) -> u64 {
        let mut fs = open_fs();
        fs.lookup(path)
            .expect("lookup")
            .unwrap_or_else(|| panic!("{path} must exist"))
    }

    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let apfs = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &apfs]).output();
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

    /// Our reader against the model: every file's bytes and size, every
    /// directory's listing, every symlink's target, every attribute.
    fn verify_model(label: &str, model: &Model) {
        let mut fs = open_fs();
        for (path, &n) in &model.files {
            let want = &model.nodes[n];
            let attr = fs
                .getattr(path)
                .expect("getattr")
                .unwrap_or_else(|| panic!("{label}: {path} must exist"));
            assert_eq!(attr.size as usize, want.len(), "{label}: {path} size");
            let got = fs.read(path, 0, want.len()).expect("read");
            assert!(got == *want, "{label}: {path} content differs");
        }
        for (path, target) in &model.symlinks {
            let got = fs.read_symlink(path).expect("readlink");
            assert_eq!(
                got.as_deref(),
                Some(target.as_slice()),
                "{label}: {path} target"
            );
        }
        let mut dirs: Vec<String> = model.dirs.iter().cloned().collect();
        dirs.push("/".into());
        for dir in dirs {
            let prefix = if dir == "/" {
                "/".to_string()
            } else {
                format!("{dir}/")
            };
            let mut want: BTreeSet<String> = BTreeSet::new();
            for p in model
                .files
                .keys()
                .chain(model.symlinks.keys())
                .chain(model.dirs.iter())
            {
                if let Some(rest) = p.strip_prefix(&prefix) {
                    if !rest.is_empty() && !rest.contains('/') {
                        want.insert(rest.to_string());
                    }
                }
            }
            let got: BTreeSet<String> = fs
                .read_dir(&dir)
                .expect("read_dir")
                .into_iter()
                .map(|e| e.name)
                .filter(|n| !n.starts_with('.'))
                .collect();
            assert_eq!(got, want, "{label}: listing of {dir}");
        }
        for (path, attrs) in &model.xattrs {
            for (name, value) in attrs {
                let got = fs.get_xattr(path, name).expect("get_xattr");
                assert_eq!(
                    got.as_deref(),
                    Some(value.as_slice()),
                    "{label}: {path} xattr {name}"
                );
            }
            let names: BTreeSet<String> = fs
                .list_xattrs(path)
                .expect("list_xattrs")
                .into_iter()
                .filter(|n| n.starts_with("user."))
                .collect();
            let want: BTreeSet<String> = attrs.keys().cloned().collect();
            assert_eq!(names, want, "{label}: {path} attribute names");
        }
    }

    fn check(label: &str, model: &Model) {
        run_fsck(label);
        verify_model(label, model);
        println!("feature-sweep: {label} ok");
    }

    /// Set a file's length the way the mount host does: the fast path first,
    /// then read-cut-rewrite when it declines.
    fn set_size(model: &mut Model, path: &str, new_size: usize) {
        let (dir, name) = Model::parent_and_name(path);
        let parent = ino(&dir);
        let current = model.content(path).len();
        let outcome = with_txn(|t, v, o| {
            truncate_file_fast(t, v, o, parent, &name, new_size as u64, current as u64)
        });
        if matches!(outcome, TruncateOutcome::FastPathDeclined) {
            let mut cut = model.content(path).to_vec();
            cut.resize(new_size, 0);
            with_txn(|t, v, o| overwrite_existing_file(t, v, o, parent, &name, &cut));
        }
        let mut c = model.content(path).to_vec();
        c.resize(new_size, 0);
        model.set_content(path, c);
    }

    fn overwrite(model: &mut Model, path: &str, content: Vec<u8>) {
        let (dir, name) = Model::parent_and_name(path);
        let parent = ino(&dir);
        with_txn(|t, v, o| overwrite_existing_file(t, v, o, parent, &name, &content));
        model.set_content(path, content);
    }

    #[test]
    #[ignore = "fsck-gated; the whole write surface against macOS-authored files"]
    fn every_operation_against_every_kind_of_file() {
        if !gate() {
            println!("feature-sweep: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let mut model = Model::default();
        build_image(&mut model);
        check("as macOS left it", &model);

        // --- Replacing content: compressed, one side of a clone, a hard link.
        overwrite(&mut model, "/compressed.bin", pattern(90 * 1024, 20));
        check("overwrite compressed", &model);
        overwrite(&mut model, "/clone_a.bin", pattern(70 * 1024, 21));
        check("overwrite one side of the clone", &model);
        // Windows replaces a file by truncating it first, then writing: the
        // truncation empties one side while the other still shares the stream.
        overwrite(&mut model, "/clone2_a.bin", Vec::new());
        check(
            "empty one side of a clone the other side still shares",
            &model,
        );
        overwrite(&mut model, "/clone2_a.bin", pattern(70_000, 23));
        check("then write it anew", &model);
        overwrite(&mut model, "/sub/link_b.txt", pattern(50 * 1024, 22));
        check("overwrite through the second hard link", &model);

        // --- Growing and cutting: plain, sparse, the clone's other side.
        set_size(&mut model, "/plain.bin", 300 * 1024);
        check("grow plain", &model);
        set_size(&mut model, "/plain.bin", 150 * 1024 + 123);
        check("cut plain inside a block", &model);
        set_size(&mut model, "/sparse.bin", 200_000);
        check("grow sparse further", &model);
        set_size(&mut model, "/sparse.bin", 5_000);
        check("cut sparse into its real bytes", &model);
        set_size(&mut model, "/clone_b.bin", 4096);
        check("cut the clone's other side", &model);
        set_size(&mut model, "/clone_b.bin", 0);
        check("cut to nothing", &model);
        set_size(&mut model, "/clone_b.bin", 65_536);
        check("grow from nothing", &model);

        // --- Attributes: ours on plain and on a clone, replacing macOS's.
        let plain_ino = ino("/plain.bin");
        with_txn(|t, v, o| set_xattr(t, v, o, plain_ino, "user.small", b"v1"));
        with_txn(|t, v, o| set_xattr(t, v, o, plain_ino, "user.stream", &pattern(120 * 1024, 30)));
        model
            .xattrs
            .entry("/plain.bin".into())
            .or_default()
            .insert("user.small".into(), b"v1".into());
        model
            .xattrs
            .get_mut("/plain.bin")
            .unwrap()
            .insert("user.stream".into(), pattern(120 * 1024, 30));
        check("set small and stream attributes", &model);
        with_txn(|t, v, o| set_xattr(t, v, o, plain_ino, "user.stream", &pattern(8 * 1024, 31)));
        model
            .xattrs
            .get_mut("/plain.bin")
            .unwrap()
            .insert("user.stream".into(), pattern(8 * 1024, 31));
        check(
            "replace a stream attribute with a bigger inline one",
            &model,
        );
        with_txn(|t, v, o| delete_xattr(t, v, o, plain_ino, "user.small"));
        model
            .xattrs
            .get_mut("/plain.bin")
            .unwrap()
            .remove("user.small");
        check("delete an attribute", &model);
        let tagged_ino = ino("/tagged.bin");
        with_txn(|t, v, o| set_xattr(t, v, o, tagged_ino, "user.large", b"shrunk"));
        model
            .xattrs
            .get_mut("/tagged.bin")
            .unwrap()
            .insert("user.large".into(), b"shrunk".into());
        with_txn(|t, v, o| delete_xattr(t, v, o, tagged_ino, "user.small"));
        model
            .xattrs
            .get_mut("/tagged.bin")
            .unwrap()
            .remove("user.small");
        check("rewrite and delete macOS attributes", &model);

        // --- Names: rename, rename over, move across directories.
        with_txn(|t, v, o| rename(t, v, o, ROOT, "plain.bin", "plain2.bin", false));
        model.rename("/plain.bin", "/plain2.bin");
        check("rename", &model);
        with_txn(|t, v, o| rename(t, v, o, ROOT, "link_a.txt", "link_a2.txt", false));
        model.rename("/link_a.txt", "/link_a2.txt");
        check("rename the primary name of a hard link", &model);
        with_txn(|t, v, o| rename(t, v, o, ROOT, "tagged.bin", "compressed.bin", true));
        model.remove("/compressed.bin");
        model.rename("/tagged.bin", "/compressed.bin");
        check("rename over an existing file", &model);
        let sub = ino("/sub");
        with_txn(|t, v, o| move_entry(t, v, o, ROOT, "sparse.bin", sub, "sparse.bin", false));
        model.rename("/sparse.bin", "/sub/sparse.bin");
        check("move into a subdirectory", &model);
        with_txn(|t, v, o| move_entry(t, v, o, sub, "link_b.txt", ROOT, "link_b.txt", false));
        model.rename("/sub/link_b.txt", "/link_b.txt");
        check("move a hard link out", &model);

        // --- Replacing what is already there: a symlink, one name of a hard
        // link, one side of a macOS clone, a file with a stream attribute.
        with_txn(|t, v, o| write_file(t, v, o, ROOT, "src1.bin", &pattern(7000, 80)));
        with_txn(|t, v, o| rename(t, v, o, ROOT, "src1.bin", "sym", true));
        model.remove("/sym");
        model.add_file("/sym", pattern(7000, 80));
        check("rename over a symlink", &model);
        with_txn(|t, v, o| write_file(t, v, o, ROOT, "src2.bin", &pattern(8000, 81)));
        with_txn(|t, v, o| rename(t, v, o, ROOT, "src2.bin", "link_b.txt", true));
        model.remove("/link_b.txt");
        model.add_file("/link_b.txt", pattern(8000, 81));
        check("rename over one name of a hard link", &model);
        // The name that survives is now the file's only one; writing through
        // it is an ordinary overwrite (Windows: WriteAllText on the survivor).
        overwrite(&mut model, "/link_a2.txt", pattern(5000, 85));
        check("overwrite the surviving name of a hard link", &model);
        with_txn(|t, v, o| write_file(t, v, o, sub, "src3.bin", &pattern(9000, 82)));
        with_txn(|t, v, o| move_entry(t, v, o, sub, "src3.bin", ROOT, "clone2_a.bin", true));
        model.remove("/clone2_a.bin");
        model.add_file("/clone2_a.bin", pattern(9000, 82));
        check("move over one side of a macOS clone", &model);
        with_txn(|t, v, o| write_file(t, v, o, ROOT, "victim.bin", &pattern(6000, 83)));
        let victim_ino = ino("/victim.bin");
        with_txn(|t, v, o| set_xattr(t, v, o, victim_ino, "user.stream", &pattern(70 * 1024, 84)));
        with_txn(|t, v, o| create_symlink(t, v, o, ROOT, "victim.bin", b"clone2_b.bin", true));
        model.remove("/victim.bin");
        model
            .symlinks
            .insert("/victim.bin".into(), b"clone2_b.bin".into());
        check("symlink over a file with a stream attribute", &model);

        // --- Links: add a name, remove the primary, remove the rest.
        with_txn(|t, v, o| link(t, v, o, ROOT, "plain2.bin", sub, "plain_link.bin"));
        model.link("/plain2.bin", "/sub/plain_link.bin");
        check("link", &model);
        with_txn(|t, v, o| unlink(t, v, o, ROOT, "plain2.bin"));
        model.remove("/plain2.bin");
        check("unlink the primary name", &model);
        with_txn(|t, v, o| unlink(t, v, o, ROOT, "link_a2.txt"));
        model.remove("/link_a2.txt");
        check("unlink one macOS link", &model);

        // --- Clones: of the macOS clone, of a hard link, of the sparse file.
        with_txn(|t, v, o| clone_file(t, v, o, ROOT, "clone_a.bin", ROOT, "clone_c.bin"));
        model.add_file("/clone_c.bin", model.content("/clone_a.bin").to_vec());
        with_txn(|t, v, o| clone_file(t, v, o, ROOT, "link_b.txt", sub, "link_copy.txt"));
        model.add_file("/sub/link_copy.txt", model.content("/link_b.txt").to_vec());
        with_txn(|t, v, o| clone_file(t, v, o, sub, "sparse.bin", ROOT, "sparse_copy.bin"));
        model.add_file(
            "/sparse_copy.bin",
            model.content("/sub/sparse.bin").to_vec(),
        );
        check("clones", &model);
        overwrite(&mut model, "/clone_c.bin", pattern(12 * 1024, 40));
        check("overwrite a clone of ours", &model);
        with_txn(|t, v, o| unlink(t, v, o, ROOT, "clone_a.bin"));
        model.remove("/clone_a.bin");
        check("unlink the clone's source", &model);

        // --- Every compression method macOS reads, under the same operations:
        // replaced, grown, cut, cloned, renamed over, unlinked, replaced by a
        // symlink, tagged.
        let cmp = ino("/cmp");
        overwrite(
            &mut model,
            "/cmp/m14_lzbitmap_rsrc_big.bin",
            pattern(50 * 1024, 90),
        );
        overwrite(
            &mut model,
            "/cmp/m13_lzbitmap_attr_text.bin",
            pattern(1000, 91),
        );
        overwrite(
            &mut model,
            "/cmp/m04_zlib_rsrc_mixed.bin",
            pattern(20 * 1024, 92),
        );
        check("overwrite LZBITMAP and zlib files", &model);
        set_size(&mut model, "/cmp/m12_lzfse_rsrc_big.bin", 400_000);
        set_size(&mut model, "/cmp/m04_zlib_rsrc_big.bin", 70_001);
        set_size(&mut model, "/cmp/m11_lzfse_attr_text.bin", 0);
        set_size(&mut model, "/cmp/m10_plain_rsrc_exact.bin", 65_537);
        check("grow and cut LZFSE, zlib and plain files", &model);
        with_txn(|t, v, o| {
            clone_file(
                t,
                v,
                o,
                cmp,
                "m08_lzvn_rsrc_text.bin",
                ROOT,
                "lzvn_copy.bin",
            )
        });
        model.add_file(
            "/lzvn_copy.bin",
            model.content("/cmp/m08_lzvn_rsrc_text.bin").to_vec(),
        );
        with_txn(|t, v, o| {
            clone_file(t, v, o, cmp, "m03_zlib_attr_text.bin", cmp, "zlib_copy.bin")
        });
        model.add_file(
            "/cmp/zlib_copy.bin",
            model.content("/cmp/m03_zlib_attr_text.bin").to_vec(),
        );
        with_txn(|t, v, o| {
            clone_file(
                t,
                v,
                o,
                cmp,
                "m14_lzbitmap_rsrc_text.bin",
                cmp,
                "lzbitmap_copy.bin",
            )
        });
        model.add_file(
            "/cmp/lzbitmap_copy.bin",
            model.content("/cmp/m14_lzbitmap_rsrc_text.bin").to_vec(),
        );
        check("clone compressed files", &model);
        overwrite(&mut model, "/lzvn_copy.bin", pattern(30 * 1024, 93));
        check("overwrite the clone of a compressed file", &model);
        with_txn(|t, v, o| {
            rename(
                t,
                v,
                o,
                cmp,
                "m10_plain_rsrc_text.bin",
                "m09_plain_attr_text.bin",
                true,
            )
        });
        model.remove("/cmp/m09_plain_attr_text.bin");
        model.rename(
            "/cmp/m10_plain_rsrc_text.bin",
            "/cmp/m09_plain_attr_text.bin",
        );
        with_txn(|t, v, o| unlink(t, v, o, cmp, "m07_lzvn_attr_random.bin"));
        model.remove("/cmp/m07_lzvn_attr_random.bin");
        with_txn(|t, v, o| {
            create_symlink(
                t,
                v,
                o,
                cmp,
                "m12_lzfse_rsrc_tiny.bin",
                b"m12_lzfse_rsrc_text.bin",
                true,
            )
        });
        model.remove("/cmp/m12_lzfse_rsrc_tiny.bin");
        model.symlinks.insert(
            "/cmp/m12_lzfse_rsrc_tiny.bin".into(),
            b"m12_lzfse_rsrc_text.bin".into(),
        );
        let lzb = ino("/cmp/m14_lzbitmap_rsrc_text.bin");
        with_txn(|t, v, o| set_xattr(t, v, o, lzb, "user.note", b"kept"));
        model
            .xattrs
            .entry("/cmp/m14_lzbitmap_rsrc_text.bin".into())
            .or_default()
            .insert("user.note".into(), b"kept".into());
        check(
            "rename over, unlink, symlink over and tag compressed files",
            &model,
        );

        // --- Symbolic links and directories.
        with_txn(|t, v, o| create_symlink(t, v, o, sub, "to_nested", b"deeper/nested.bin", false));
        model
            .symlinks
            .insert("/sub/to_nested".into(), b"deeper/nested.bin".into());
        with_txn(|t, v, o| create_symlink(t, v, o, ROOT, "sym", b"clone_c.bin", true));
        model.remove("/sym");
        model.symlinks.insert("/sym".into(), b"clone_c.bin".into());
        check("symbolic links", &model);
        with_txn(|t, v, o| mkdir(t, v, o, sub, "newdir"));
        model.dirs.insert("/sub/newdir".into());
        let newdir = ino("/sub/newdir");
        with_txn(|t, v, o| write_file(t, v, o, newdir, "fresh.bin", &pattern(3000, 50)));
        model.add_file("/sub/newdir/fresh.bin", pattern(3000, 50));
        check("new directory with a file", &model);
        with_txn(|t, v, o| unlink(t, v, o, newdir, "fresh.bin"));
        model.remove("/sub/newdir/fresh.bin");
        with_txn(|t, v, o| unlink(t, v, o, sub, "newdir"));
        model.remove("/sub/newdir");
        check("remove the file and the directory", &model);

        // --- Snapshots: take, mutate, delete; take, mutate, revert.
        let s1 = with_txn(|t, v, o| create_snapshot(t, v, o, "sweep-1"));
        check("snapshot taken", &model);
        overwrite(&mut model, "/compressed.bin", pattern(40 * 1024, 60));
        with_txn(|t, v, o| unlink(t, v, o, ROOT, "sparse_copy.bin"));
        model.remove("/sparse_copy.bin");
        check("mutated under a snapshot", &model);
        with_txn(|t, v, o| delete_snapshot(t, v, o, s1));
        check("snapshot deleted", &model);

        let frozen = model.clone();
        let s2 = with_txn(|t, v, o| create_snapshot(t, v, o, "sweep-2"));
        overwrite(&mut model, "/clone_b.bin", pattern(9 * 1024, 70));
        with_txn(|t, v, o| write_file(t, v, o, ROOT, "after_snapshot.bin", &pattern(5000, 71)));
        model.add_file("/after_snapshot.bin", pattern(5000, 71));
        with_txn(|t, v, o| unlink(t, v, o, ROOT, "link_b.txt"));
        model.remove("/link_b.txt");
        check("mutated under the second snapshot", &model);
        with_txn(|t, v, o| revert_to_snapshot(t, v, o, s2));
        model = frozen;
        check("reverted", &model);

        // --- macOS is the last judge: it mounts the result and reads it all.
        let (m, parent) = attach(true);
        let guard = Attached(parent);
        assert!(!m.is_empty(), "the result must mount on macOS");
        let mut problems: Vec<String> = Vec::new();
        for (path, &n) in &model.files {
            match std::fs::read(format!("{m}{path}")) {
                Ok(got) if got == model.nodes[n] => {}
                Ok(_) => problems.push(format!("{path}: macOS read different bytes")),
                Err(e) => problems.push(format!("{path}: macOS could not read it: {e}")),
            }
        }
        for (path, target) in &model.symlinks {
            match std::fs::read_link(format!("{m}{path}")) {
                Ok(t) if t.as_os_str().as_encoded_bytes() == target.as_slice() => {}
                other => problems.push(format!("{path}: macOS readlink gave {other:?}")),
            }
        }
        for (path, attrs) in &model.xattrs {
            for (name, value) in attrs {
                // xattr(1) prints values as text or a hex dump; the system
                // call gives the bytes themselves.
                let out = Command::new("python3")
                    .args([
                        "-c",
                        "import ctypes, sys\n\
                         libc = ctypes.CDLL(None, use_errno=True)\n\
                         buf = ctypes.create_string_buffer(4 << 20)\n\
                         n = libc.getxattr(sys.argv[1].encode(), sys.argv[2].encode(), buf, len(buf), 0, 0)\n\
                         if n < 0: sys.exit(ctypes.get_errno() or 1)\n\
                         sys.stdout.buffer.write(buf.raw[:n])",
                        &format!("{m}{path}"),
                        name,
                    ])
                    .output()
                    .expect("getxattr via python");
                if !out.status.success() {
                    problems.push(format!("{path}: macOS could not read attribute {name}"));
                } else if out.stdout != *value {
                    problems.push(format!("{path}: macOS reads attribute {name} differently"));
                }
            }
        }
        drop(guard);
        assert!(
            problems.is_empty(),
            "macOS disagrees:\n  {}",
            problems.join("\n  ")
        );
        run_fsck("after macOS mounted it");
        println!("feature-sweep PASS: every operation against every kind of file");
    }
}
