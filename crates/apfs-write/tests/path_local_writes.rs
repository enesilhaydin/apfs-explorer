//! Path-local tree writes against the whole-tree rebuild.
//!
//! One seeded sequence of operations runs on three copies of a scratch image:
//! one rebuilds every tree an operation changes (the reference), one rewrites
//! only the nodes on the paths to the changed records, and one does the same
//! with many operations to a transaction, where a node an earlier operation
//! wrote is rewritten in place. After each phase every volume must pass fsck
//! and the writer's own catalog check, and
//! read back as the same tree: names, kinds, sizes, link counts, contents and
//! extended attributes. The sequence grows the catalog past one level and the
//! volume object map past one node, then removes most of it again, so nodes
//! split, empty and drop on the way.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]`: runs fsck, slow.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test path_local_writes -- --ignored --nocapture --test-threads=1
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
mod path_local {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        clone_file, create_file, delete_xattr, link, mkdir, move_entry, overwrite_existing_file,
        rename, set_inode_attrs, set_xattr, truncate_file_fast, unlink, verify_fsroot,
    };
    use apfs_write::txn::{Transaction, TxnError};
    use std::collections::BTreeMap;
    use std::process::Command;

    const REBUILT: &str = "/tmp/apfs-pathlocal-rebuilt.img";
    const PATHS: &str = "/tmp/apfs-pathlocal-paths.img";
    /// Path-local too, `GROUP_OPS` operations to a transaction.
    const GROUPED: &str = "/tmp/apfs-pathlocal-grouped.img";
    const GROUP_OPS: usize = 25;
    const SIZE_MB: u32 = 256;
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT_INO: u64 = 2;
    /// Files created in the growing phase.
    const GROW: usize = 2000;
    /// Mixed operations after it.
    const MIX: usize = 900;
    const SIZES: [usize; 7] = [0, 1, 4096, 5000, 12_288, 20_000, 70_000];

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn root() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }

    fn gen_scratch(img: &str) {
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                img,
                &SIZE_MB.to_string(),
            ])
            .current_dir(root())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }

    fn fsck(img: &str) -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "verify-scratch", img])
            .current_dir(root())
            .output()
            .expect("verify-scratch");
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }

    fn open_dev(img: &str) -> Dev {
        let mut head = vec![0u8; 64 * 1024];
        std::io::Read::read_exact(&mut std::fs::File::open(img).unwrap(), &mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let e = &head[base + i * sz..base + (i + 1) * sz];
            if e[..16] == APFS_GPT_TYPE {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                let inner = FileBlockDeviceRw::open_rw(img).unwrap();
                return WritableOffsetBlockDevice::new(inner, s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition in {img}");
    }

    fn vsb_omap(dev: &mut Dev) -> (Vec<u8>, Vec<u8>) {
        let c = Container::open(dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(dev, fs_oid, nx.obj.xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }

    /// One operation: the transaction, the volume superblock and its
    /// object-map header.
    type Op<'a, T> = dyn FnMut(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, TxnError> + 'a;

    /// Run `f` in one committed transaction on `img`, rebuilding trees in
    /// full or not.
    fn run<T>(img: &str, full: bool, f: &mut Op<'_, T>) -> Result<T, TxnError> {
        let mut dev = open_dev(img);
        let (vsb, om) = vsb_omap(&mut dev);
        let mut txn = Transaction::begin(dev)?;
        txn.set_full_rebuild(full);
        match f(&mut txn, &vsb, &om) {
            Ok(v) => {
                txn.commit()?;
                Ok(v)
            }
            Err(e) => {
                txn.abort();
                Err(e)
            }
        }
    }

    /// The open transaction on the grouped image, with the superblock and
    /// object-map header read before it began, and the operations in it.
    struct Group {
        txn: Transaction<Dev>,
        vsb: Vec<u8>,
        om: Vec<u8>,
        ops: usize,
    }

    thread_local! {
        static GROUP: std::cell::RefCell<Option<Group>> = const { std::cell::RefCell::new(None) };
    }

    /// Commit the grouped image's open transaction, if any.
    fn close_group() {
        if let Some(g) = GROUP.with(|g| g.borrow_mut().take()) {
            g.txn.commit().expect("grouped commit");
        }
    }

    /// Run `f` in the grouped image's open transaction, opening one if
    /// needed and committing it every `GROUP_OPS` operations. Every
    /// operation is given the copies read before the transaction began.
    fn grouped<T>(f: &mut Op<'_, T>) -> Result<T, TxnError> {
        let out = GROUP.with(|cell| {
            let mut cell = cell.borrow_mut();
            if cell.is_none() {
                let mut dev = open_dev(GROUPED);
                let (vsb, om) = vsb_omap(&mut dev);
                let txn = Transaction::begin(dev).expect("grouped begin");
                *cell = Some(Group {
                    txn,
                    vsb,
                    om,
                    ops: 0,
                });
            }
            let g = cell.as_mut().expect("just opened");
            g.ops += 1;
            (f(&mut g.txn, &g.vsb, &g.om), g.ops >= GROUP_OPS)
        });
        if out.1 {
            close_group();
        }
        out.0
    }

    /// Run `f` on all three images; they must agree on the outcome. An
    /// operation the reference refuses is not run on the grouped image: it
    /// changes nothing, and a refused operation inside a group would take
    /// the group's earlier operations down with it.
    fn both<T: PartialEq + std::fmt::Debug>(
        what: &str,
        mut f: impl FnMut(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, TxnError>,
    ) -> Option<T> {
        let a = run(REBUILT, true, &mut f);
        let b = run(PATHS, false, &mut f);
        match (a, b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a, b, "{what}: results differ");
                let c = grouped(&mut f).unwrap_or_else(|e| panic!("{what}: grouped gave {e:?}"));
                assert_eq!(a, c, "{what}: grouped result differs");
                Some(a)
            }
            (Err(a), Err(b)) => {
                assert_eq!(
                    std::mem::discriminant(&a),
                    std::mem::discriminant(&b),
                    "{what}: errors differ: {a:?} / {b:?}"
                );
                None
            }
            (a, b) => panic!("{what}: rebuild gave {a:?}, path-local gave {b:?}"),
        }
    }

    /// What a path holds, as the reader sees it (no timestamps).
    #[derive(Debug, PartialEq, Eq)]
    struct Seen {
        is_dir: bool,
        size: u64,
        links: u32,
        mode: u16,
        content: u64,
        xattrs: Vec<(String, u64)>,
    }

    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    fn tree(img: &str) -> BTreeMap<String, Seen> {
        let mut fs = apfs::FsView::open(open_dev(img)).unwrap();
        let mut out = BTreeMap::new();
        let mut todo = vec!["/".to_string()];
        while let Some(dir) = todo.pop() {
            for e in fs.read_dir(&dir).unwrap() {
                let path = if dir == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{dir}/{}", e.name)
                };
                let a = fs.getattr(&path).unwrap().unwrap();
                let content = if a.is_dir {
                    todo.push(path.clone());
                    0
                } else {
                    fnv(&fs.read(&path, 0, a.size as usize + 1).unwrap())
                };
                let mut names = fs.list_xattrs(&path).unwrap();
                names.sort();
                let xattrs = names
                    .into_iter()
                    .map(|n| {
                        let v = fs.get_xattr(&path, &n).unwrap().unwrap_or_default();
                        (n, fnv(&v))
                    })
                    .collect();
                out.insert(
                    path,
                    Seen {
                        is_dir: a.is_dir,
                        size: a.size,
                        links: a.nchildren_or_nlink,
                        mode: a.mode,
                        content,
                        xattrs,
                    },
                );
            }
        }
        out
    }

    /// The catalog root's level and the volume object map root's level.
    fn levels(img: &str) -> (u16, u16) {
        let mut dev = open_dev(img);
        let (vsb, om) = vsb_omap(&mut dev);
        let bsz = vsb.len() as u64;
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let vol_omap = Omap::open(&mut dev, v.omap_oid, bsz as u32).unwrap();
        let root = vol_omap
            .resolve(&mut dev, v.root_tree_oid, v.obj.xid, bsz as u32)
            .unwrap()
            .unwrap();
        let mut node = vec![0u8; bsz as usize];
        dev.read_at(root * bsz, &mut node).unwrap();
        let cat = u16::from_le_bytes(node[34..36].try_into().unwrap());
        let tree_paddr = u64::from_le_bytes(om[48..56].try_into().unwrap());
        dev.read_at(tree_paddr * bsz, &mut node).unwrap();
        let omap = u16::from_le_bytes(node[34..36].try_into().unwrap());
        (cat, omap)
    }

    fn own_check(img: &str) {
        let mut dev = open_dev(img);
        let (vsb, om) = vsb_omap(&mut dev);
        let bsz = vsb.len();
        let mut txn = Transaction::begin(dev).unwrap();
        let r = verify_fsroot(&mut txn, &vsb, &om, bsz);
        txn.abort();
        r.unwrap_or_else(|e| panic!("{img}: catalog check: {e}"));
    }

    fn checkpoint(phase: &str) {
        close_group();
        for img in [REBUILT, PATHS, GROUPED] {
            own_check(img);
            let (ok, out) = fsck(img);
            assert!(ok, "{phase}: {img} is not fsck-clean:\n{out}");
        }
        let a = tree(REBUILT);
        for img in [PATHS, GROUPED] {
            let b = tree(img);
            assert_eq!(a.len(), b.len(), "{phase}: {img}: entry counts differ");
            for (path, seen) in &a {
                assert_eq!(Some(seen), b.get(path), "{phase}: {img}: {path} differs");
            }
        }
        println!(
            "[{phase}] {} entries, levels (catalog, omap): rebuilt {:?}, path-local {:?}, grouped {:?}",
            a.len(),
            levels(REBUILT),
            levels(PATHS),
            levels(GROUPED)
        );
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    struct File {
        dir: u64,
        name: String,
        ino: u64,
        size: usize,
    }

    fn body(seed: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| ((i * 31 + seed * 7) % 251) as u8)
            .collect()
    }

    fn name(rng: &mut Rng, n: usize) -> String {
        // Short names, and every third or so a long one: the long ones move
        // the longest key and fill nodes faster, so the trees grow levels.
        let pad = if rng.below(3) == 0 {
            120 + rng.below(100)
        } else {
            rng.below(12)
        };
        format!("f{n:05}{}", "x".repeat(pad))
    }

    #[test]
    #[ignore = "fsck-gated; slow"]
    fn path_local_writes_match_the_rebuild() {
        if !gate() {
            return;
        }
        gen_scratch(REBUILT);
        std::fs::copy(REBUILT, PATHS).unwrap();
        std::fs::copy(REBUILT, GROUPED).unwrap();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut dirs = vec![ROOT_INO];
        let mut files: Vec<File> = Vec::new();
        let mut serial = 0usize;

        // Grow: directories, files, contents, attributes.
        for i in 0..GROW {
            if i % 100 == 0 {
                let d = both("mkdir", |t, v, o| {
                    mkdir(t, v, o, ROOT_INO, &format!("d{i}"))
                })
                .expect("mkdir");
                dirs.push(d);
            }
            let dir = dirs[rng.below(dirs.len())];
            serial += 1;
            let n = name(&mut rng, serial);
            let ino = both("create", |t, v, o| create_file(t, v, o, dir, &n)).expect("create");
            let mut size = 0;
            if rng.below(3) == 0 {
                size = SIZES[rng.below(SIZES.len())];
                let data = body(serial, size);
                both("write", |t, v, o| {
                    overwrite_existing_file(t, v, o, dir, &n, &data)
                });
            }
            if rng.below(10) == 0 {
                let len = if rng.below(2) == 0 { 40 } else { 5000 };
                let val = body(serial + 1, len);
                both("set_xattr", |t, v, o| {
                    set_xattr(t, v, o, ino, "user.test", &val)
                });
            }
            files.push(File {
                dir,
                name: n,
                ino,
                size,
            });
        }
        checkpoint("grow");
        let (cat, omap) = levels(PATHS);
        assert!(cat >= 1, "the catalog never split (level {cat})");
        assert!(omap >= 1, "the object map never split (level {omap})");

        // Mix: renames, rewrites, truncations, links, attributes, removals.
        for _ in 0..MIX {
            if files.is_empty() {
                break;
            }
            let k = rng.below(files.len());
            match rng.below(11) {
                0 => {
                    serial += 1;
                    let new = name(&mut rng, serial);
                    let f = &files[k];
                    if both("rename", |t, v, o| {
                        rename(t, v, o, f.dir, &f.name, &new, false)
                    })
                    .is_some()
                    {
                        files[k].name = new;
                    }
                }
                1 => {
                    let size = SIZES[rng.below(SIZES.len())];
                    let f = &files[k];
                    let data = body(serial + k, size);
                    both("rewrite", |t, v, o| {
                        overwrite_existing_file(t, v, o, f.dir, &f.name, &data)
                    });
                    files[k].size = size;
                }
                2 => {
                    let f = &files[k];
                    let new_size = f.size / 2;
                    both("truncate", |t, v, o| {
                        truncate_file_fast(t, v, o, f.dir, &f.name, new_size as u64, f.size as u64)
                            .map(|_| ())
                    });
                    files[k].size = new_size;
                }
                3 => {
                    serial += 1;
                    let dst = name(&mut rng, serial);
                    let ddir = dirs[rng.below(dirs.len())];
                    let f = &files[k];
                    if both("link", |t, v, o| link(t, v, o, f.dir, &f.name, ddir, &dst)).is_some() {
                        let (ino, size) = (f.ino, f.size);
                        files.push(File {
                            dir: ddir,
                            name: dst,
                            ino,
                            size,
                        });
                    }
                }
                7 => {
                    serial += 1;
                    let new = name(&mut rng, serial);
                    let ddir = dirs[rng.below(dirs.len())];
                    let f = &files[k];
                    if both("move", |t, v, o| {
                        move_entry(t, v, o, f.dir, &f.name, ddir, &new, false)
                    })
                    .is_some()
                    {
                        files[k].dir = ddir;
                        files[k].name = new;
                    }
                }
                8 => {
                    serial += 1;
                    let dst = name(&mut rng, serial);
                    let ddir = dirs[rng.below(dirs.len())];
                    let f = &files[k];
                    if let Some(ino) = both("clone", |t, v, o| {
                        clone_file(t, v, o, f.dir, &f.name, ddir, &dst)
                    }) {
                        let size = f.size;
                        files.push(File {
                            dir: ddir,
                            name: dst,
                            ino,
                            size,
                        });
                    }
                }
                9 => {
                    let f = &files[k];
                    let when = 1_700_000_000_000_000_000 + rng.next() % 1_000_000_000;
                    both("set_inode_attrs", |t, v, o| {
                        set_inode_attrs(
                            t,
                            v,
                            o,
                            f.dir,
                            &f.name,
                            None,
                            None,
                            Some(when),
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                    });
                }
                4 => {
                    let f = &files[k];
                    if rng.below(2) == 0 {
                        let val = body(k, 300);
                        both("set_xattr", |t, v, o| {
                            set_xattr(t, v, o, f.ino, "user.mix", &val)
                        });
                    } else {
                        both("delete_xattr", |t, v, o| {
                            delete_xattr(t, v, o, f.ino, "user.mix")
                        });
                    }
                }
                _ => {
                    let f = files.swap_remove(k);
                    both("unlink", |t, v, o| unlink(t, v, o, f.dir, &f.name));
                }
            }
        }
        checkpoint("mix");

        // Shrink: remove most of what is left, so nodes empty and drop.
        while files.len() > 40 {
            let k = rng.below(files.len());
            let f = files.swap_remove(k);
            both("unlink", |t, v, o| unlink(t, v, o, f.dir, &f.name));
        }
        checkpoint("shrink");
        let _ = std::fs::remove_file(REBUILT);
        let _ = std::fs::remove_file(PATHS);
        let _ = std::fs::remove_file(GROUPED);
    }
}
