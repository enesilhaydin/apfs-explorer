//! #157 regression: write operations on a volume whose VOLUME OMAP has split
//! into a MULTI-NODE b-tree must stay correct + fsck-clean.
//!
//! Root cause (found while chasing #156): the write path's `omap_lookup`
//! (used by `collect_fstree`) and `build_omap_node` (used by `rewrite_fstree`)
//! both assume a SINGLE-NODE volume omap. Once the volume has enough catalog +
//! extentref nodes that the omap b-tree splits (empirically ~3-4k files), the
//! reader misinterprets the omap ROOT (an index node) as a leaf and reads a
//! garbage paddr ("read out of range: offset ~2^44 ..."), and the rebuilder
//! cannot emit a multi-node omap. The real-USB #156 volume had only 250 files
//! (single-node omap, omap tree level 0) so it never hit this - #157 is a
//! distinct bug. Currently FAILS at the FIRST write op (clean Err, not a
//! corruption: large-file-count volumes are effectively write-blocked).
//!
//! This test populates enough macOS-authored files to force a multi-node volume
//! omap (asserts the omap tree root level > 0 - else it is vacuous), then runs a
//! small create/rename/unlink workload and fscks. RED today; GREEN once the
//! write path descends + rebuilds a multi-node omap.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - macOS populate + fsck, slow.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test omap_multinode_157 \
//!     -- --ignored --nocapture --test-threads=1
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
mod r157 {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_file, mkdir, move_entry, rename, unlink, verify_fsroot, write_file,
    };
    use apfs_write::snapshot::{create_snapshot, delete_snapshot, revert_to_snapshot};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-157-multinode-omap.img";
    const SIZE_MB: u32 = 512;
    // Enough macOS-authored files to split the VOLUME omap into a multi-node
    // b-tree. Verified at runtime via the omap-tree-level assertion below.
    const N_FILES: usize = 4000;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const SECTOR: u64 = 512;

    /// The first line of an fsck report worth acting on, for stage-by-stage
    /// output: without it a late failure gives no hint which step introduced it.
    fn first_complaint(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| {
                l.contains("corrupt")
                    || l.contains("invalid")
                    || l.contains("overallocation")
                    || l.contains("is not valid")
            })
            .take(2)
            .collect()
    }
    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                &SIZE_MB.to_string(),
            ])
            .current_dir(root())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn macos_populate(count: usize) {
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "populate-scratch",
                IMG,
                &count.to_string(),
            ])
            .current_dir(root())
            .status()
            .expect("populate-scratch");
        assert!(s.success(), "populate-scratch failed");
    }
    fn verify() -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "verify-scratch", IMG])
            .current_dir(root())
            .output()
            .expect("verify-scratch");
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }
    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        let pe = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let np = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let ps = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let eo = (pe * SECTOR) as usize;
        for i in 0..np {
            let off = eo + i * ps;
            let e = match raw.get(off..off + ps) {
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
        let xid = nx.obj.xid;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }
    fn open_txn() -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }
    /// Volume omap b-tree root node level (0 = single node; >0 = multi-node).
    fn vol_omap_tree_level() -> u16 {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let (bsz, vsb_omap_oid) = {
            let c = Container::open(&mut dev).unwrap();
            let nx = &c.superblock;
            let bsz = nx.block_size;
            let omap = Omap::open(&mut dev, nx.omap_oid, bsz).unwrap();
            let fs_oid = *nx.fs_oids.first().unwrap();
            let vp = omap
                .resolve(&mut dev, fs_oid, nx.obj.xid, bsz)
                .unwrap()
                .unwrap();
            let mut vsb_buf = vec![0u8; bsz as usize];
            dev.read_at(vp * bsz as u64, &mut vsb_buf).unwrap();
            (bsz, VolumeSuperblock::parse(&vsb_buf).unwrap().omap_oid)
        };
        let mut omap_phys = vec![0u8; bsz as usize];
        dev.read_at(vsb_omap_oid * bsz as u64, &mut omap_phys)
            .unwrap();
        let tree_paddr = u64::from_le_bytes(omap_phys[48..56].try_into().unwrap());
        let mut node = vec![0u8; bsz as usize];
        dev.read_at(tree_paddr * bsz as u64, &mut node).unwrap();
        u16::from_le_bytes(node[34..36].try_into().unwrap())
    }
    fn ext_for(i: usize) -> &'static str {
        match i % 5 {
            0 => "txt",
            1 => "dat",
            2 => "log",
            3 => "md",
            _ => "bin",
        }
    }
    fn mac_name(i: usize) -> String {
        format!("u{i:03}.{}", ext_for(i))
    }

    /// DEBUG (not a gate): raw-decode the volume omap b-tree structure of the
    /// image at $APFS157_DUMP_IMG, to compare a macOS-authored (clean) omap
    /// against our rebuilt one. Resolves the volume omap_phys via apfs-core, then
    /// hand-decodes the tree nodes (so it also works on a structurally-broken omap
    /// that BtreeNode::parse would reject).
    #[test]
    #[ignore = "debug dumper; set APFS157_DUMP_IMG"]
    fn dump_volume_omap_structure() {
        let img = match std::env::var("APFS157_DUMP_IMG") {
            Ok(p) => p,
            Err(_) => {
                println!("set APFS157_DUMP_IMG");
                return;
            }
        };
        let raw = std::fs::read(&img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(&img).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let (bsz, vsb_omap_oid) = {
            let c = Container::open(&mut dev).unwrap();
            let nx = &c.superblock;
            let bsz = nx.block_size;
            let omap = Omap::open(&mut dev, nx.omap_oid, bsz).unwrap();
            let fs_oid = *nx.fs_oids.first().unwrap();
            let vp = omap
                .resolve(&mut dev, fs_oid, nx.obj.xid, bsz)
                .unwrap()
                .unwrap();
            let mut b = vec![0u8; bsz as usize];
            dev.read_at(vp * bsz as u64, &mut b).unwrap();
            (bsz, VolumeSuperblock::parse(&b).unwrap().omap_oid)
        };
        let bsz = bsz as usize;
        let mut omap_phys = vec![0u8; bsz];
        dev.read_at(vsb_omap_oid * bsz as u64, &mut omap_phys)
            .unwrap();
        let tree_root = u64::from_le_bytes(omap_phys[48..56].try_into().unwrap());
        println!("=== volume omap: phys@{vsb_omap_oid} tree_root@{tree_root} ===");

        let rd16 = |b: &[u8], o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        let rd32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());

        let mut decode = |dev: &mut WritableOffsetBlockDevice<FileBlockDeviceRw>,
                          paddr: u64,
                          tag: &str|
         -> Vec<u64> {
            let mut n = vec![0u8; bsz];
            dev.read_at(paddr * bsz as u64, &mut n).unwrap();
            let otype = rd32(&n, 24);
            let osub = rd32(&n, 28);
            let flags = rd16(&n, 32);
            let level = rd16(&n, 34);
            let nkeys = rd32(&n, 36) as usize;
            let ts_off = rd16(&n, 40) as usize;
            let ts_len = rd16(&n, 42) as usize;
            let fs_off = rd16(&n, 44);
            let fs_len = rd16(&n, 46);
            let is_root = flags & 1 != 0;
            let is_leaf = level == 0;
            let key_area = 56 + ts_off + ts_len;
            let val_area_end = bsz - if is_root { 40 } else { 0 };
            println!("[{tag}] paddr={paddr} type={otype:#x} sub={osub:#x} flags={flags:#x} level={level} nkeys={nkeys} ts_off={ts_off} ts_len={ts_len} fs_off={fs_off} fs_len={fs_len} val_area_end={val_area_end}");
            if is_root {
                let bti = bsz - 40;
                println!("    footer: lk={} lv={} key_count={} node_count={} bt_flags={:#x} ksz={} vsz={}",
                    rd32(&n, bti + 16), rd32(&n, bti + 20), rd64(&n, bti + 24), rd64(&n, bti + 32),
                    rd32(&n, bti), rd32(&n, bti + 8), rd32(&n, bti + 12));
            }
            let vsz = if is_leaf { 16 } else { 8 };
            let mut children = Vec::new();
            let show: Vec<usize> = if nkeys <= 4 {
                (0..nkeys).collect()
            } else {
                vec![0, 1, nkeys - 2, nkeys - 1]
            };
            for i in show {
                let te = 56 + ts_off + i * 4;
                let k_off = rd16(&n, te) as usize;
                let v_off = rd16(&n, te + 2) as usize;
                let k = key_area + k_off;
                let (oid, xid) = (rd64(&n, k), rd64(&n, k + 8));
                let v = val_area_end - v_off;
                if is_leaf {
                    println!("    [{i}] k=(oid={oid},xid={xid}) k_off={k_off} v_off={v_off} ov_flags={:#x} ov_paddr={}", rd32(&n, v), rd64(&n, v + 8));
                } else {
                    let child = rd64(&n, v);
                    println!("    [{i}] k=(oid={oid},xid={xid}) k_off={k_off} v_off={v_off} child_paddr={child}");
                }
            }
            if !is_leaf {
                for i in 0..nkeys {
                    let te = 56 + ts_off + i * 4;
                    let v_off = rd16(&n, te + 2) as usize;
                    children.push(rd64(&n, val_area_end - v_off));
                }
            }
            children
        };

        let kids = decode(&mut dev, tree_root, "ROOT");
        if let Some(&first) = kids.first() {
            let gk = decode(&mut dev, first, "child[0]");
            if let Some(&gg) = gk.first() {
                decode(&mut dev, gg, "child[0][0]");
            }
        }
        if let Some(&last) = kids.last() {
            decode(&mut dev, last, "child[last]");
        }
    }

    /// Forensic: run `verify_fsroot` against an arbitrary APFS-in-GPT image given
    /// by `$APFS_VERIFY_IMG` (e.g. a `dd` of a corrupt real disk). Prints the
    /// stats on success or the FIRST structural violation our checker finds - used
    /// to (a) decode a reproduced #156 corruption and (b) confirm whether our
    /// Windows-side checker actually detects it (vs an fsck-only finding = checker
    /// gap). Not a gate; opt-in via the env var.
    #[test]
    #[ignore = "forensic; set APFS_VERIFY_IMG to an APFS-in-GPT image"]
    fn verify_image_from_env() {
        let img = match std::env::var("APFS_VERIFY_IMG") {
            Ok(p) => p,
            Err(_) => {
                println!("set APFS_VERIFY_IMG=<path to an APFS-in-GPT image>");
                return;
            }
        };
        let raw = std::fs::read(&img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(&img).unwrap();
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut txn = Transaction::begin(dev).unwrap();
        let (vsb_raw, omap_raw) = {
            // Read vsb + omap_phys for the first volume via the container omap.
            let raw2 = std::fs::read(&img).unwrap();
            let (b2, l2) = find_apfs_range(&raw2);
            let inner2 = FileBlockDeviceRw::open_rw(&img).unwrap();
            let mut dev2 = WritableOffsetBlockDevice::new(inner2, b2, l2);
            let c = Container::open(&mut dev2).unwrap();
            let nx = &c.superblock;
            let bsz = nx.block_size as usize;
            let omap = Omap::open(&mut dev2, nx.omap_oid, nx.block_size).unwrap();
            let fs_oid = *nx.fs_oids.first().unwrap();
            let vp = omap
                .resolve(&mut dev2, fs_oid, nx.obj.xid, nx.block_size)
                .unwrap()
                .unwrap();
            let mut vsb = vec![0u8; bsz];
            dev2.read_at(vp * bsz as u64, &mut vsb).unwrap();
            let v = VolumeSuperblock::parse(&vsb).unwrap();
            let mut om = vec![0u8; bsz];
            dev2.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
            (vsb, om)
        };
        let bsz = vsb_raw.len();
        match verify_fsroot(&mut txn, &vsb_raw, &omap_raw, bsz) {
            Ok(s) => println!(
                "[verify-image] verify_fsroot OK: {} nodes, {} records",
                s.nodes, s.records
            ),
            Err(e) => println!("[verify-image] verify_fsroot VIOLATION: {e}"),
        }
    }

    #[test]
    #[ignore = "fsck-gated; slow (macOS populate of a multi-node-omap volume + fsck)"]
    fn writes_on_multinode_omap_stay_fsck_clean() {
        if !check_gate() {
            println!("#157: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        macos_populate(N_FILES);
        let (ok0, _) = verify();
        assert!(
            ok0,
            "#157 precondition: {N_FILES} macOS files must be fsck-clean"
        );

        // Precondition: the VOLUME omap MUST be multi-node, else this test does
        // not exercise the descent/rebuild paths and proves nothing.
        let omap_level = vol_omap_tree_level();
        println!("[#157] volume omap tree level after {N_FILES} files = {omap_level}");
        assert!(
            omap_level > 0,
            "#157: volume omap is single-node (level {omap_level}) after {N_FILES} files - \
             bump N_FILES until the omap splits, else this test is vacuous"
        );

        let mut fails: Vec<String> = Vec::new();

        // A long workload that GROWS then CHURNS the omap so its node count
        // changes across ops - this is what stresses fs_alloc_count / node-delta
        // accounting (a per-op drift in the omap-tree node count accumulates and
        // surfaces as an fsck alloc-count error at the end). One op per commit.
        // (1) 60 creates (grow the omap - likely adds leaves / splits further).
        for i in 0..60 {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let name = format!("new_{i:04}.txt");
            match create_file(&mut txn, &vsb, &om, 2, &name) {
                Ok(_) => txn
                    .commit()
                    .map(|_| ())
                    .unwrap_or_else(|e| fails.push(format!("create {name} COMMIT-ERR {e}"))),
                Err(e) => {
                    let _ = txn.abort();
                    fails.push(format!("create {name} OP-ERR {e}"));
                }
            }
        }
        // (2) 30 renames of existing macOS files.
        for i in 0..30 {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let old = mac_name(i);
            let new = format!("ren_{i:04}.{}", ext_for(i));
            match rename(&mut txn, &vsb, &om, 2, &old, &new, false) {
                Ok(()) => txn
                    .commit()
                    .map(|_| ())
                    .unwrap_or_else(|e| fails.push(format!("rename {old} COMMIT-ERR {e}"))),
                Err(e) => {
                    let _ = txn.abort();
                    fails.push(format!("rename {old} OP-ERR {e}"));
                }
            }
        }
        // (3) 20 unlinks of existing macOS files (shrink the omap).
        for i in 100..120 {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let name = mac_name(i);
            match unlink(&mut txn, &vsb, &om, 2, &name) {
                Ok(()) => txn
                    .commit()
                    .map(|_| ())
                    .unwrap_or_else(|e| fails.push(format!("unlink {name} COMMIT-ERR {e}"))),
                Err(e) => {
                    let _ = txn.abort();
                    fails.push(format!("unlink {name} OP-ERR {e}"));
                }
            }
        }

        // Cross-check: our Windows-runnable structural self-check must AGREE the
        // multi-node tree is clean (no false positive on a tree we built/edited).
        {
            let (vsb, om) = read_vsb_omap();
            let bsz = vsb.len();
            let mut txn = open_txn();
            match verify_fsroot(&mut txn, &vsb, &om, bsz) {
                Ok(s) => println!(
                    "[#157] verify_fsroot OK on clean multi-node tree: {} nodes, {} records",
                    s.nodes, s.records
                ),
                Err(e) => fails.push(format!("verify_fsroot FALSE POSITIVE on clean tree: {e}")),
            }
        }

        let (ok, out) = verify();
        let verdict: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.contains("corrupt")
                    || l.contains("invalid")
                    || l.contains("overallocation")
                    || l.contains("is not valid")
            })
            .collect();
        println!("[#157] post-workload fsck_clean={ok}  {verdict:?}  op_fails={fails:?}");
        assert!(
            fails.is_empty(),
            "#157: write ops on a multi-node-omap volume failed:\n  {}",
            fails.join("\n  ")
        );
        assert!(
            ok,
            "#157: multi-node-omap workload left the volume fsck-DIRTY: {verdict:?}"
        );
        println!("#157 PASS: writes on a multi-node volume omap stay fsck-clean");
    }

    /// `move_entry` (cross-directory move) keeps its own copy of the volume-omap
    /// rebuild, which the multi-node work never reached: it decodes only the
    /// omap ROOT and writes the result back through the single-node builder. On
    /// a volume whose omap has split, that reads an index root as if it were a
    /// leaf and emits one node in place of the whole tree, orphaning every real
    /// leaf. The rename path above proves the shared rebuild handles this shape;
    /// this proves the move path does too.
    #[test]
    #[ignore = "fsck-gated; slow (macOS populate of a multi-node-omap volume + fsck)"]
    fn move_entry_on_multinode_omap_stays_fsck_clean() {
        if !check_gate() {
            println!("#157-move: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        macos_populate(N_FILES);
        let (ok0, _) = verify();
        assert!(
            ok0,
            "#157-move precondition: populated volume must be clean"
        );

        let omap_level = vol_omap_tree_level();
        assert!(
            omap_level > 0,
            "#157-move: volume omap is single-node (level {omap_level}) - test would be vacuous"
        );

        let mut fails: Vec<String> = Vec::new();

        // Destination directory for the moves.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            match mkdir(&mut txn, &vsb, &om, 2, "moved") {
                Ok(_) => txn
                    .commit()
                    .map(|_| ())
                    .unwrap_or_else(|e| fails.push(format!("mkdir COMMIT-ERR {e}"))),
                Err(e) => {
                    let _ = txn.abort();
                    fails.push(format!("mkdir OP-ERR {e}"));
                }
            }
        }
        let dst_ino = {
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = apfs::FsView::open(dev).expect("FsView::open");
            fs.lookup("/moved").expect("lookup").expect("moved dir")
        };

        // Move a batch of the macOS-authored files into it, one op per commit.
        for i in 0..20 {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let name = mac_name(i);
            match move_entry(&mut txn, &vsb, &om, 2, &name, dst_ino, &name, false) {
                Ok(()) => txn
                    .commit()
                    .map(|_| ())
                    .unwrap_or_else(|e| fails.push(format!("move {name} COMMIT-ERR {e}"))),
                Err(e) => {
                    let _ = txn.abort();
                    fails.push(format!("move {name} OP-ERR {e}"));
                }
            }
        }

        let (ok, out) = verify();
        let verdict: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.contains("corrupt")
                    || l.contains("invalid")
                    || l.contains("overallocation")
                    || l.contains("is not valid")
            })
            .collect();
        println!("[#157-move] fsck_clean={ok} {verdict:?} op_fails={fails:?}");
        assert!(
            fails.is_empty(),
            "#157-move: move_entry failed on a multi-node-omap volume:\n  {}",
            fails.join("\n  ")
        );
        assert!(
            ok,
            "#157-move: move_entry left the volume fsck-DIRTY: {verdict:?}"
        );
        println!("#157-move PASS: move_entry on a multi-node volume omap stays fsck-clean");
    }

    /// Snapshots carried their own copy of the volume-omap rebuild, and it was
    /// the last one still written for a single node: it decoded the omap root
    /// as a leaf, so on a split omap it would emit one node in place of the
    /// whole tree and orphan every real leaf. Rather than corrupt the volume it
    /// refused outright, which left a volume of this size unable to take a
    /// snapshot at all.
    ///
    /// Deleting the snapshot afterwards is deliberately NOT asserted here - see
    /// `delete_snapshot_on_a_populated_volume_is_fsck_clean`.
    #[test]
    #[ignore = "fsck-gated; slow (macOS populate of a multi-node-omap volume + fsck)"]
    fn snapshot_and_write_on_multinode_omap_stay_fsck_clean() {
        if !check_gate() {
            println!("#157-snap: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        macos_populate(N_FILES);
        let (ok0, _) = verify();
        assert!(
            ok0,
            "#157-snap precondition: populated volume must be clean"
        );

        let omap_level = vol_omap_tree_level();
        assert!(
            omap_level > 0,
            "#157-snap: volume omap is single-node (level {omap_level}) - test would be vacuous"
        );

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            create_snapshot(&mut txn, &vsb, &om, "snap-157").expect("create_snapshot");
            txn.commit().expect("commit snapshot");
        }
        {
            let (ok, out) = verify();
            assert!(
                ok,
                "#157-snap: create_snapshot alone left the volume dirty: {:?}",
                first_complaint(&out)
            );
        }
        // A write under the snapshot, so the delete has copied-away blocks to
        // reclaim rather than an untouched volume.
        let body = vec![0xA7u8; 40 * 1024];
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, "snap_157_file.bin", &body).expect("write");
            txn.commit().expect("commit write");
        }
        {
            let (ok, out) = verify();
            assert!(
                ok,
                "#157-snap: writing under the snapshot left the volume dirty: {:?}",
                first_complaint(&out)
            );
        }
        // The file written under the snapshot must read back.
        {
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = apfs::FsView::open(dev).expect("FsView::open");
            let got = fs.read("/snap_157_file.bin", 0, body.len()).expect("read");
            assert!(got == body, "#157-snap: content changed under the snapshot");
        }
        println!("#157-snap PASS: snapshot + write on a multi-node volume omap are clean");
    }

    /// Writing under a snapshot and then deleting it is where three separate
    /// faults met, none of them reachable on a volume without history: the
    /// delete walked a b-tree root it had staged but not written and freed the
    /// file data it found there instead of nodes, and the object map kept the
    /// versions the snapshot had pinned, both as superseded copies and as nodes
    /// the live catalog no longer names.
    ///
    /// Set `APFS_STAGE_COPY=1` to keep a copy of the image after the populate
    /// and after the write; comparing an allocation bitmap across those and the
    /// final state is what localised the first of the three.
    #[test]
    #[ignore = "fsck-gated; slow (macOS populate of a multi-node-omap volume + fsck)"]
    fn write_under_snapshot_then_delete_is_fsck_clean() {
        if !check_gate() {
            println!("#157-del: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        macos_populate(N_FILES);
        let (ok0, _) = verify();
        assert!(ok0, "#157-del precondition: populated volume must be clean");
        if std::env::var("APFS_STAGE_COPY").is_ok() {
            std::fs::copy(IMG, "/tmp/stage-after-populate.img").unwrap();
        }

        let snap_xid = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let xid =
                create_snapshot(&mut txn, &vsb, &om, "snap-157-del").expect("create_snapshot");
            txn.commit().expect("commit snapshot");
            xid
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(
                &mut txn,
                &vsb,
                &om,
                2,
                "under_snap.bin",
                &[0xB4u8; 40 * 1024],
            )
            .expect("write under snapshot");
            txn.commit().expect("commit write");
        }
        {
            let (ok, out) = verify();
            assert!(
                ok,
                "#157-del: the write under the snapshot is itself clean: {:?}",
                first_complaint(&out)
            );
        }
        if std::env::var("APFS_STAGE_COPY").is_ok() {
            std::fs::copy(IMG, "/tmp/stage-after-write.img").unwrap();
        }
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            delete_snapshot(&mut txn, &vsb, &om, snap_xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }

        let (ok, out) = verify();
        let verdict = first_complaint(&out);
        assert!(
            ok && verdict.is_empty(),
            "#157-del: delete after a write under the snapshot left the volume \
             fsck-DIRTY: {verdict:?}"
        );
        println!("#157-del PASS: write under a snapshot then delete is clean");
    }

    /// The revert path shared the same single-node assumption and refused a
    /// split omap outright. Asserted twice over: the rebuilt map must still
    /// describe the whole catalog, and fsck must accept the result.
    #[test]
    #[ignore = "fsck-gated; slow (macOS populate of a multi-node-omap volume)"]
    fn revert_on_multinode_omap_rebuilds_the_whole_map() {
        if !check_gate() {
            println!("#157-revert: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        macos_populate(N_FILES);
        let (ok0, _) = verify();
        assert!(
            ok0,
            "#157-revert precondition: populated volume must be clean"
        );
        assert!(
            vol_omap_tree_level() > 0,
            "#157-revert: volume omap is single-node - test would be vacuous"
        );

        let snap_xid = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let xid =
                create_snapshot(&mut txn, &vsb, &om, "snap-157-rev").expect("create_snapshot");
            txn.commit().expect("commit snapshot");
            xid
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            create_file(&mut txn, &vsb, &om, 2, "after_snap.txt").expect("create after snap");
            txn.commit().expect("commit create");
        }
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            revert_to_snapshot(&mut txn, &vsb, &om, snap_xid).expect("revert_to_snapshot");
            txn.commit().expect("commit revert");
        }

        // Reverting undoes the post-snapshot create.
        {
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = apfs::FsView::open(dev).expect("FsView::open");
            assert!(
                fs.lookup("/after_snap.txt").expect("lookup").is_none(),
                "#157-revert: the file created after the snapshot must be gone"
            );
        }

        // The object map must still describe the whole catalog: a rebuild that
        // collapsed the tree to one node would orphan most of it, which the
        // fsroot walk detects immediately.
        {
            let (vsb, om) = read_vsb_omap();
            let bsz = vsb.len();
            let mut txn = open_txn();
            let stats = verify_fsroot(&mut txn, &vsb, &om, bsz)
                .unwrap_or_else(|e| panic!("#157-revert: fsroot unreachable after revert: {e}"));
            assert!(
                stats.nodes > 1 && stats.records > N_FILES as u64,
                "#157-revert: catalog shrank to {} nodes / {} records - the omap \
                 rebuild dropped most of the map",
                stats.nodes,
                stats.records
            );
            println!(
                "[#157-revert] catalog after revert: {} nodes, {} records",
                stats.nodes, stats.records
            );
        }
        assert!(
            vol_omap_tree_level() > 0,
            "#157-revert: the rebuilt omap collapsed to a single node"
        );
        let (ok, out) = verify();
        let verdict = first_complaint(&out);
        assert!(
            ok && verdict.is_empty(),
            "#157-revert: revert left the volume fsck-DIRTY: {verdict:?}"
        );
        println!("#157-revert PASS: revert rebuilds the whole multi-node volume omap");
    }

    /// Proves `verify_fsroot` actually DETECTS the #156-class drift (not a no-op
    /// that always returns Ok): on a clean tree it passes, then we bump the
    /// fsroot root footer `bt_key_count` by +1 on disk and it must flag the
    /// mismatch. (`read_block` does not validate the Fletcher checksum, so the
    /// validator parses the tampered block and catches the count drift without a
    /// checksum fix.) A single-node tree exercises the same validator path.
    #[test]
    #[ignore = "fsck-gated; slow (macOS populate)"]
    fn verify_fsroot_detects_footer_drift() {
        if !check_gate() {
            println!("#157: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        macos_populate(250);

        // Clean tree → verify_fsroot must pass, and resolve the fsroot root paddr.
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let (bsz, root_paddr) = {
            let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
            let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
            let (bsz, vsbp) = {
                let c = Container::open(&mut dev).unwrap();
                let nx = &c.superblock;
                let bsz = nx.block_size;
                let omap = Omap::open(&mut dev, nx.omap_oid, bsz).unwrap();
                let fs_oid = *nx.fs_oids.first().unwrap();
                let vp = omap
                    .resolve(&mut dev, fs_oid, nx.obj.xid, bsz)
                    .unwrap()
                    .unwrap();
                let mut b = vec![0u8; bsz as usize];
                dev.read_at(vp * bsz as u64, &mut b).unwrap();
                (bsz, VolumeSuperblock::parse(&b).unwrap())
            };
            let vol_omap = Omap::open(&mut dev, vsbp.omap_oid, bsz).unwrap();
            let rp = vol_omap
                .resolve(&mut dev, vsbp.root_tree_oid, vsbp.obj.xid, bsz)
                .unwrap()
                .expect("fsroot root resolves");
            (bsz as usize, rp)
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            verify_fsroot(&mut txn, &vsb, &om, bsz)
                .expect("verify_fsroot must pass on the clean tree");
        }

        // Corrupt: +1 to bt_key_count in the fsroot ROOT btree_info footer.
        let file_off = base as usize + root_paddr as usize * bsz + (bsz - 40) + 24;
        let mut img = std::fs::read(IMG).unwrap();
        let kc = u64::from_le_bytes(img[file_off..file_off + 8].try_into().unwrap());
        img[file_off..file_off + 8].copy_from_slice(&(kc + 1).to_le_bytes());
        std::fs::write(IMG, &img).unwrap();

        // verify_fsroot MUST now flag the bt_key_count drift.
        let (vsb, om) = read_vsb_omap();
        let mut txn = open_txn();
        match verify_fsroot(&mut txn, &vsb, &om, bsz) {
            Ok(_) => panic!("verify_fsroot FAILED to detect bt_key_count drift"),
            Err(e) => {
                assert!(e.contains("bt_key_count"), "unexpected error: {e}");
                println!("[#157] verify_fsroot correctly detected drift: {e}");
            }
        }
    }
}
