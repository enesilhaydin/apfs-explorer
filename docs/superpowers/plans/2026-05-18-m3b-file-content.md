# M3b — Uncompressed File Content Read Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`). **Lean consolidated process (CLAUDE.md "Süreç (konsolide)"):** per task = implementer (TDD red→green, self-test once) + ONE consolidated independent review (spec + quality + gates; reviewer reads the diff and runs gates, does not trust the implementer report). Milestone end = 1 final review + 1 decisive Python cross-check. No separate spec/quality/final per task.

**Goal:** Read the byte content of an uncompressed regular file from an APFS volume, proven on the real `apfs-large.img` fixture (`dir00/f000.bin` == 4096 bytes of `'A'`) with an independent Python cross-check.

**Architecture:** Pure-Rust, `#![forbid(unsafe_code)]`. New `inode.rs` parses the packed `j_inode_val_t` prefix (xfields begin at byte 92) and walks the xfields blob (`xf_blob_t` header → `x_field_t` array → data area, each field's slot `round_up(x_size, 8)`) to extract the `INO_EXT_TYPE_DSTREAM` `j_dstream_t` (authoritative logical size). `catalog.rs` gains `Catalog::read_file`: locate the `APFS_TYPE_INODE` record for the inode number, then enumerate `APFS_TYPE_FILE_EXTENT` records keyed by the inode's `private_id` (the data-stream oid), assemble extents by `logical_addr`, truncate to `dstream.size`. Reuses the existing M3a `Catalog::for_each_leaf` tree walk.

**Tech Stack:** Rust (`apfs-core`), `thiserror`, macOS `hdiutil` fixture (`cargo run -p xtask -- gen-fixture --large`), Python 3 stdlib for the independent cross-check.

**Spec sources (locked; query before coding):** `ctx_search(source:"apfs-spec:m3b-confirmed")` (j_inode_val packed offsets @92, xfields walk `round_up(x_size,8)`, x_field_t 4B, j_dstream_t 5×u64, FILE_EXTENT keyed by private_id, the read algorithm), `apfs-spec:m3-confirmed` (catalog tree / JKey / FILE_EXTENT val layout), `apfs-spec:m1c-confirmed` (btree node layout), `apfs-spec:pdf`.

**Out of scope (M3c, do NOT implement here):** decmpfs / `com.apple.decmpfs` / ZLIB/LZVN/LZFSE decompression; xattr (`j_xattr`) reading; Finder metadata; symlink target resolution; sparse-file semantics beyond zero-fill of uncovered ranges; resource forks.

**Mandatory rules:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (no `cd`); NEVER `git add -A` / `git add .` — explicit filenames only; code/comments/commit messages in English; `apfs-core` stays `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic!`/raw indexing in production paths (test modules use `#[cfg(test)]` + the established `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]` placed once between `#[cfg(test)]` and `mod tests {` — check whether the target file's test module already has it; add it scoped to test only if absent, never duplicated, never leaked to production); clippy `--all-targets --all-features -- -D warnings` green; commit trailer (exact last line) `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m3b-file-content`.

**Gate command:**
```
cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace && \
cargo clippy --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace --all-targets --all-features -- -D warnings && \
cargo fmt --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --all --check
```

---

## File Structure

- **Create** `crates/apfs-core/src/inode.rs` — `JDstream` (5×u64 data-stream record), `Inode` (packed `j_inode_val_t` prefix + xfields walk), `INO_EXT_TYPE_DSTREAM`. Pure parsing, no I/O, fully unit-testable with synthetic bytes.
- **Modify** `crates/apfs-core/src/catalog.rs` — add `pub fn read_file` to `impl Catalog` (two `for_each_leaf` passes: inode then extents) + a decisive real-image test.
- **Modify** `crates/apfs-core/src/lib.rs` — `pub mod inode; pub use inode::{Inode, JDstream};`.

Existing M3a API reused (do NOT modify): `Catalog { /* private */ }` with `fn for_each_leaf<D:BlockDevice>(&self, dev:&mut D, node:&BtreeNode, depth:u8, f:&mut dyn FnMut(&[u8],&[u8])->Result<(),ContainerError>)->Result<(),ContainerError>` (private), `pub fn list_dir<D>(&self, dev:&mut D, dir_inode:u64, names_hashed:bool)->Result<Vec<DirEntry>,ContainerError>`, `DirEntry{ name:String, file_id:u64, flags:u16 }`, module-private `fn oor()->ContainerError`, private field `block_size:u32`, private `root:BtreeNode` (Clone). `jkey`: `JKey{obj_id:u64,obj_type:u8}`, `JKey::parse`, `APFS_TYPE_INODE=3`, `APFS_TYPE_FILE_EXTENT=8`, `ROOT_DIR_INO_NUM=2`. `endian`: `u16_le`, `u32_le`, `u64_le`, `ParseError`.

---

## Task 1: inode.rs — JDstream + Inode (packed prefix @92 + xfields walk)

**Files:**
- Create: `crates/apfs-core/src/inode.rs`
- Modify: `crates/apfs-core/src/lib.rs`

- [ ] **Step 1: Create the module with failing tests**

Create `crates/apfs-core/src/inode.rs` with EXACTLY:

```rust
//! j_inode_val_t (packed) + xfields walk. Offsets/rule:
//! ctx_search(source:"apfs-spec:m3b-confirmed"). xfields[] starts at byte 92;
//! each extended field's data slot is round_up(x_size, 8) bytes.
use crate::endian::{u16_le, u64_le, ParseError};

/// Extended-field type: the data stream descriptor (j_dstream_t). Apple p110.
pub const INO_EXT_TYPE_DSTREAM: u8 = 8;
/// Byte offset of xfields[] inside the packed j_inode_val_t.
pub const INODE_XFIELDS_OFFSET: usize = 92;

/// j_dstream_t — a file's data stream record (all u64, 40 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JDstream {
    pub size: u64,
    pub alloced_size: u64,
    pub default_crypto_id: u64,
    pub total_bytes_written: u64,
    pub total_bytes_read: u64,
}

impl JDstream {
    pub fn parse(b: &[u8]) -> Result<Self, ParseError> {
        Ok(Self {
            size: u64_le(b, 0)?,
            alloced_size: u64_le(b, 8)?,
            default_crypto_id: u64_le(b, 16)?,
            total_bytes_written: u64_le(b, 24)?,
            total_bytes_read: u64_le(b, 32)?,
        })
    }
}

/// Parsed inode value: the fields M3b needs plus the data stream (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    pub parent_id: u64,
    pub private_id: u64,
    pub internal_flags: u64,
    pub dstream: Option<JDstream>,
}

#[inline]
fn round_up8(n: usize) -> usize {
    (n + 7) & !7
}

impl Inode {
    /// Parse a j_inode_val_t value buffer. The xfields blob (if present)
    /// begins at INODE_XFIELDS_OFFSET (92): xf_blob_t { u16 xf_num_exts;
    /// u16 xf_used_data; } then xf_num_exts x_field_t {u8 x_type; u8 x_flags;
    /// u16 x_size} entries, then the data area; each field's data occupies
    /// round_up(x_size, 8) bytes in x_field order.
    pub fn parse(val: &[u8]) -> Result<Self, ParseError> {
        let parent_id = u64_le(val, 0)?;
        let private_id = u64_le(val, 8)?;
        let internal_flags = u64_le(val, 48)?;
        let mut dstream = None;
        if val.len() > INODE_XFIELDS_OFFSET {
            let num = u16_le(val, INODE_XFIELDS_OFFSET)? as usize;
            // xf_used_data is val[94..96]; not needed for the walk.
            let arr = INODE_XFIELDS_OFFSET + 4; // start of x_field_t array
            let data0 = arr
                .checked_add(num.checked_mul(4).ok_or(ParseError::Short {
                    at: arr,
                    need: 0,
                    len: val.len(),
                })?)
                .ok_or(ParseError::Short {
                    at: arr,
                    need: 0,
                    len: val.len(),
                })?;
            let mut cur = 0usize;
            for i in 0..num {
                let ent = arr + i * 4;
                let x_type = *val.get(ent).ok_or(ParseError::Short {
                    at: ent,
                    need: 1,
                    len: val.len(),
                })?;
                let x_size = u16_le(val, ent + 2)? as usize;
                if x_type == INO_EXT_TYPE_DSTREAM {
                    let start = data0.checked_add(cur).ok_or(ParseError::Short {
                        at: data0,
                        need: cur,
                        len: val.len(),
                    })?;
                    let field = val.get(start..start + x_size).ok_or(ParseError::Short {
                        at: start,
                        need: x_size,
                        len: val.len(),
                    })?;
                    dstream = Some(JDstream::parse(field)?);
                    break;
                }
                cur = cur
                    .checked_add(round_up8(x_size))
                    .ok_or(ParseError::Short {
                        at: cur,
                        need: x_size,
                        len: val.len(),
                    })?;
            }
        }
        Ok(Self {
            parent_id,
            private_id,
            internal_flags,
            dstream,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn base_prefix(private_id: u64) -> Vec<u8> {
        let mut v = vec![0u8; INODE_XFIELDS_OFFSET];
        v[0..8].copy_from_slice(&7u64.to_le_bytes()); // parent_id = 7
        v[8..16].copy_from_slice(&private_id.to_le_bytes());
        v[48..56].copy_from_slice(&0u64.to_le_bytes()); // internal_flags
        v
    }

    #[test]
    fn no_xfields_means_no_dstream() {
        let v = base_prefix(0x1234);
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.parent_id, 7);
        assert_eq!(ino.private_id, 0x1234);
        assert!(ino.dstream.is_none());
    }

    #[test]
    fn single_dstream_xfield() {
        let mut v = base_prefix(0xABCD);
        // xf_blob: num=1, used=4+round_up(40,8)=44
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&44u16.to_le_bytes());
        // x_field[0] = { x_type=8 (DSTREAM), x_flags=0, x_size=40 }
        v.push(INO_EXT_TYPE_DSTREAM);
        v.push(0);
        v.extend_from_slice(&40u16.to_le_bytes());
        // data area (starts at 96 + 1*4 = offset 100): j_dstream_t, size=4096
        let mut ds = vec![0u8; 40];
        ds[0..8].copy_from_slice(&4096u64.to_le_bytes());
        v.extend_from_slice(&ds);
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.private_id, 0xABCD);
        let d = ino.dstream.expect("dstream present");
        assert_eq!(d.size, 4096);
    }

    #[test]
    fn dstream_after_unaligned_first_field_uses_round_up_8() {
        // Two xfields: [0]=NAME type 4 x_size=3 (slot round_up(3,8)=8),
        // [1]=DSTREAM type 8 x_size=40. DSTREAM data must be read at
        // data0 + 8 (NOT data0 + 3) — proves the round_up(x_size,8) rule.
        let mut v = base_prefix(0x55);
        v.extend_from_slice(&2u16.to_le_bytes()); // xf_num_exts = 2
        v.extend_from_slice(&(8u16 + 4 + 40).to_le_bytes()); // xf_used_data (cosmetic)
        v.push(4); // x_field[0].x_type = INO_EXT_TYPE_NAME
        v.push(0);
        v.extend_from_slice(&3u16.to_le_bytes()); // x_size = 3
        v.push(INO_EXT_TYPE_DSTREAM); // x_field[1].x_type = 8
        v.push(0);
        v.extend_from_slice(&40u16.to_le_bytes()); // x_size = 40
        // data area starts at 96 + 2*4 = offset 104.
        v.extend_from_slice(&[b'a', b'b', 0]); // field0 raw value (3 bytes)
        v.extend_from_slice(&[0u8; 5]); // padding so field0 slot = round_up(3,8)=8
        let mut ds = vec![0u8; 40];
        ds[0..8].copy_from_slice(&4096u64.to_le_bytes());
        v.extend_from_slice(&ds); // field1 (DSTREAM) at data0 + 8
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.dstream.expect("dstream").size, 4096);
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        assert!(Inode::parse(&[0u8; 16]).is_err());
        assert!(JDstream::parse(&[0u8; 8]).is_err());
    }
}
```

Add to `crates/apfs-core/src/lib.rs` immediately after the line `pub use catalog::{Catalog, DirEntry};`:

```rust

pub mod inode;
pub use inode::{Inode, JDstream};
```

- [ ] **Step 2: Run tests — verify they fail before wiring**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core inode`
Expected: compile failure until `pub mod inode;` is added; after the lib.rs edit it compiles and all 4 tests PASS. (Acceptable TDD red for a new additive module per project style.)

- [ ] **Step 3: Run the full gate**

Run the **Gate command**. Expected: all green; apfs-core test count = previous 26 + 4 = 30.

- [ ] **Step 4: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/inode.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3b inode — j_inode_val packed prefix + xfields walk + j_dstream

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: catalog.rs — Catalog::read_file + decisive real-image test

**Files:**
- Modify: `crates/apfs-core/src/catalog.rs`
- Modify: `crates/apfs-core/src/lib.rs` (no change needed if Task 1 already re-exported inode; this task only adds the test + method)

- [ ] **Step 1: Ensure the populated fixture exists**

Run: `cargo run --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p xtask -- gen-fixture --large`
Expected: `tests/fixtures/apfs-large.img` exists, populated (`dir00`..`dir15`, each with `f000.bin`..`f063.bin`, every file 4096 bytes of `0x41`). macOS only; if `hdiutil attach` falls back the image is size-only and the decisive test will print `skip:` — re-run on a macOS host where attach works.

- [ ] **Step 2: Add `read_file` and the failing decisive test**

In `crates/apfs-core/src/catalog.rs`, inside `impl Catalog { ... }` add this method directly after the existing `list_dir` method:

```rust
    /// Read the full uncompressed content of the regular file with the given
    /// inode number. Finds the APFS_TYPE_INODE record, reads its data stream
    /// (INO_EXT_TYPE_DSTREAM) for the authoritative logical size, then
    /// assembles the APFS_TYPE_FILE_EXTENT records keyed by the inode's
    /// private_id (the data-stream oid), ordered by logical_addr, and
    /// truncates to the logical size. Uncovered ranges read back as zeros.
    /// (decmpfs-compressed files are out of scope — M3c.)
    pub fn read_file<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<u8>, ContainerError> {
        use crate::inode::Inode;
        use crate::jkey::{APFS_TYPE_FILE_EXTENT, APFS_TYPE_INODE};

        // Pass 1: the inode record value.
        let mut inode_val: Option<Vec<u8>> = None;
        let root = self.root.clone();
        self.for_each_leaf(dev, &root, 0, &mut |k, v| {
            if inode_val.is_none() {
                let jk = JKey::parse(k)?;
                if jk.obj_type == APFS_TYPE_INODE && jk.obj_id == inode_num {
                    inode_val = Some(v.to_vec());
                }
            }
            Ok(())
        })?;
        let inode = Inode::parse(&inode_val.ok_or_else(oor)?)?;
        let size = inode.dstream.as_ref().map(|d| d.size).unwrap_or(0) as usize;

        // Pass 2: file extents keyed by the data-stream oid (private_id).
        let pid = inode.private_id;
        let mut extents: Vec<(u64, u64, u64)> = Vec::new(); // (logical_addr, len, paddr)
        let root2 = self.root.clone();
        self.for_each_leaf(dev, &root2, 0, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type == APFS_TYPE_FILE_EXTENT && jk.obj_id == pid {
                let logical = u64::from_le_bytes(
                    k.get(8..16).ok_or_else(oor)?.try_into().map_err(|_| oor())?,
                );
                let lf = u64::from_le_bytes(
                    v.get(0..8).ok_or_else(oor)?.try_into().map_err(|_| oor())?,
                );
                let paddr = u64::from_le_bytes(
                    v.get(8..16).ok_or_else(oor)?.try_into().map_err(|_| oor())?,
                );
                extents.push((logical, lf & 0x00ff_ffff_ffff_ffff, paddr));
            }
            Ok(())
        })?;
        extents.sort_by_key(|e| e.0);

        let mut out = vec![0u8; size];
        for (logical, len, paddr) in extents {
            let logical = logical as usize;
            if logical >= size {
                continue;
            }
            let want = std::cmp::min(len as usize, size - logical);
            let mut tmp = vec![0u8; len as usize];
            dev.read_at(
                paddr
                    .checked_mul(self.block_size as u64)
                    .ok_or_else(oor)?,
                &mut tmp,
            )?;
            out.get_mut(logical..logical + want)
                .ok_or_else(oor)?
                .copy_from_slice(tmp.get(..want).ok_or_else(oor)?);
        }
        Ok(out)
    }
```

In the existing `#[cfg(test)] mod tests` block in `crates/apfs-core/src/catalog.rs`, add this test function:

```rust
    #[test]
    fn reads_real_file_content_4096_a() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let vol = vols.first().expect("user volume").clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();

        let root = cat
            .list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed)
            .expect("list root");
        let dir00 = match root.iter().find(|e| e.name == "dir00") {
            Some(d) => d.clone(),
            None => {
                eprintln!(
                    "skip: size-only fixture (hdiutil attach fell back) — \
                     re-run `cargo run -p xtask -- gen-fixture --large` on a macOS host"
                );
                return;
            }
        };
        let kids = cat
            .list_dir(&mut dev, dir00.file_id, hashed)
            .expect("list dir00");
        let f000 = kids
            .iter()
            .find(|e| e.name == "f000.bin")
            .expect("f000.bin entry");

        let content = cat
            .read_file(&mut dev, f000.file_id)
            .expect("read f000.bin");
        assert_eq!(content.len(), 4096, "f000.bin logical size");
        assert!(
            content.iter().all(|&b| b == b'A'),
            "f000.bin must be 4096 bytes of 0x41"
        );
    }
```

- [ ] **Step 3: Run the decisive test — confirm red then green**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core reads_real_file_content_4096_a -- --nocapture`
Expected: before adding `read_file` it fails to compile (`no method read_file`); after the method is added the test PASSES **un-skipped** (no `skip:` line) on this macOS host with the populated fixture. If an assertion fails, there is a real defect — debug and FIX the implementation; do NOT weaken the assertions. If it prints `skip: size-only`, re-run Step 1 and retry.

- [ ] **Step 4: Run the full gate**

Run the **Gate command**. Expected: all green; apfs-core test count = 30 + 1 = 31; `reads_real_file_content_4096_a` passes un-skipped.

- [ ] **Step 5: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/catalog.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3b catalog — Catalog::read_file (inode + dstream + file extents)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Milestone end — independent Python cross-check + final review + merge

**Files:** none modified (verification + branch finish + ROADMAP).

- [ ] **Step 1: Independent Python cross-check (decisive, offset-precise)**

Shares NO code with Rust. Run via `ctx_execute(language:"python", code: <below>)` (only the printed verdict enters context). It independently resolves the volume, walks the catalog (variable-KV, virtual children via the volume omap), finds `dir00/f000.bin`, parses its inode (packed prefix, xfields @92, `round_up(x_size,8)`, DSTREAM size), gathers FILE_EXTENT records by `private_id`, reads the bytes, and independently recomputes Fletcher-64 of the inode's leaf node.

```python
import struct
IMG="/Users/eiaydin/Documents/projects/apfs-explorer/tests/fixtures/apfs-large.img"
f=open(IMG,"rb"); BS=4096
def rd(b): f.seek(b*BS); return f.read(BS)
def u16(b,o): return struct.unpack_from("<H",b,o)[0]
def u32(b,o): return struct.unpack_from("<I",b,o)[0]
def u64(b,o): return struct.unpack_from("<Q",b,o)[0]
def i64(b,o): return struct.unpack_from("<q",b,o)[0]
def fl64(bl):
    M=0xFFFFFFFF; s1=s2=0; bd=bl[8:]; n=len(bd)-len(bd)%4
    for i in range(0,n,4):
        s1=(s1+struct.unpack_from("<I",bd,i)[0])%M; s2=(s2+s1)%M
    c1=M-((s1+s2)%M); c2=M-((s1+c1)%M); return (c2<<32)|c1
def hdr(b):
    fl=u16(b,32)
    return dict(b=b,leaf=bool(fl&2),root=bool(fl&1),fixed=bool(fl&4),
                n=u32(b,36),toff=u16(b,40),tlen=u16(b,42))
def ent(nd,i):
    base=56+nd["toff"]; ka=56+nd["toff"]+nd["tlen"]
    ve=len(nd["b"])-(40 if nd["root"] else 0)
    if nd["fixed"]:
        ko,vo=struct.unpack_from("<HH",nd["b"],base+i*4); return ka+ko,None,ve-vo,None
    ko,kl,vo,vl=struct.unpack_from("<HHHH",nd["b"],base+i*8); return ka+ko,kl,ve-vo,vl
def omap_resolve(omap_blk,toid,txid):
    tree=u64(rd(omap_blk),48); blk=tree
    for _ in range(32):
        nd=hdr(rd(blk))
        if nd["leaf"]:
            best=None
            for i in range(nd["n"]):
                ks,_,vs,_=ent(nd,i); o=u64(nd["b"],ks); x=u64(nd["b"],ks+8)
                if o==toid and x<=txid and (best is None or x>best[0]): best=(x,vs)
            if best is None: return None
            vs=best[1]
            if u32(nd["b"],vs)&1: return None
            return i64(nd["b"],vs+8)
        cand=None
        for i in range(nd["n"]):
            ks,_,vs,_=ent(nd,i); o=u64(nd["b"],ks); x=u64(nd["b"],ks+8)
            if (o,x)<=(toid,txid) and (cand is None or (o,x)>cand[0]): cand=((o,x),vs)
        blk=u64(nd["b"],ent(nd,0)[2] if cand is None else cand[1])
    return None
def walk(rootp,pred,out):
    st=[(rootp,0)]
    while st:
        blk,d=st.pop()
        if d>32: continue
        nd=hdr(rd(blk))
        for i in range(nd["n"]):
            ks,kl,vs,vl=ent(nd,i)
            if nd["leaf"]:
                pred(nd["b"],ks,kl,vs,vl,out)
            else:
                co=u64(nd["b"],vs); cp=omap_resolve(V_OMAP,co,V_XID)
                assert cp,f"child {co}"; st.append((cp,d+1))
nx=rd(0); nx_omap=u64(nx,160); nx_xid=u64(nx,16); fs0=u64(nx,184)
volp=omap_resolve(nx_omap,fs0,nx_xid); vb=rd(volp)
assert u32(vb,32)==0x42535041
V_OMAP=u64(vb,128); v_root=u64(vb,136); V_XID=u64(vb,16)
incompat=u64(vb,56); hashed=(incompat&(0x01|0x08))!=0
rootp=omap_resolve(V_OMAP,v_root,V_XID); assert rootp
# find dir00 (parent=2), then f000.bin (parent=dir00 inode)
def drec(b,ks,kl,vs,vl,out):
    idt=u64(b,ks); oid=idt&0x0fffffffffffffff; t=idt>>60
    if t!=9 or oid!=out["parent"]: return
    if hashed: nl=u32(b,ks+8)&0x3ff; noff=ks+12
    else: nl=u16(b,ks+8); noff=ks+10
    nm=b[noff:noff+nl].split(b"\x00")[0].decode("utf-8","replace")
    out["map"][nm]=u64(b,vs)
o={"parent":2,"map":{}}; walk(rootp,drec,o); d00=o["map"]["dir00"]
o={"parent":d00,"map":{}}; walk(rootp,drec,o); f000=o["map"]["f000.bin"]
# inode record for f000 (type=3)
ino={"id":f000,"val":None,"leafblk":None}
def find_ino(b,ks,kl,vs,vl,out):
    idt=u64(b,ks); oid=idt&0x0fffffffffffffff; t=idt>>60
    if t==3 and oid==out["id"] and out["val"] is None:
        out["val"]=bytes(b[vs:vs+vl])
walk(rootp,find_ino,ino)
iv=ino["val"]; private_id=u64(iv,8)
# xfields @92: xf_blob(4) + num*x_field(4) + data; slot=round_up(x_size,8)
num=u16(iv,92); arr=96; data0=arr+num*4; cur=0; size=None
for i in range(num):
    xt=iv[arr+i*4]; xs=u16(iv,arr+i*4+2)
    if xt==8:
        ds=iv[data0+cur:data0+cur+xs]; size=u64(ds,0); break
    cur+=(xs+7)&~7
assert size is not None,"no DSTREAM xfield"
# file extents keyed by private_id (type=8)
ex={"pid":private_id,"lst":[]}
def find_ext(b,ks,kl,vs,vl,out):
    idt=u64(b,ks); oid=idt&0x0fffffffffffffff; t=idt>>60
    if t==8 and oid==out["pid"]:
        la=u64(b,ks+8); lf=u64(b,vs)&0x00ffffffffffffff; pb=u64(b,vs+8)
        out["lst"].append((la,lf,pb))
walk(rootp,find_ext,ex); ex["lst"].sort()
buf=bytearray(size)
for la,ln,pb in ex["lst"]:
    if la>=size: continue
    w=min(ln,size-la); f.seek(pb*BS); chunk=f.read(ln); buf[la:la+w]=chunk[:w]
assert size==4096, f"size={size}"
assert len(buf)==4096 and all(x==0x41 for x in buf), "content not 4096x 'A'"
# independent Fletcher-64 of the inode's leaf node
def leafblk_of(rootp,target):
    st=[(rootp,0)]
    while st:
        blk,d=st.pop(); nd=hdr(rd(blk))
        for i in range(nd["n"]):
            ks,kl,vs,vl=ent(nd,i)
            if nd["leaf"]:
                idt=u64(nd["b"],ks)
                if (idt>>60)==3 and (idt&0x0fffffffffffffff)==target:
                    return nd["b"]
            else:
                co=u64(nd["b"],vs); cp=omap_resolve(V_OMAP,co,V_XID); st.append((cp,d+1))
    return None
lb=leafblk_of(rootp,f000); assert lb and u64(lb,0)==fl64(lb),"inode leaf Fletcher-64 mismatch"
print(f"PYTHON-CROSSCHECK OK | f000.bin inode={f000} private_id={private_id} "
      f"dstream.size={size} extents={len(ex['lst'])} content=4096x0x41 verified | "
      f"inode-leaf Fletcher-64 verified | hashed={hashed}")
f.close()
```

Expected output (only this line): `PYTHON-CROSSCHECK OK | f000.bin inode=... private_id=... dstream.size=4096 extents=1 content=4096x0x41 verified | inode-leaf Fletcher-64 verified | hashed=True`. If the Python disagrees with the Rust test, STOP — there is an offset/algorithm defect; investigate before merging.

- [ ] **Step 2: Final consolidated milestone review**

Dispatch ONE independent reviewer over the whole `feat/m3b-file-content` branch vs `main`. The reviewer reads the full diff, runs the **Gate command**, runs the Task 3 Step 1 Python cross-check, and verifies against `apfs-spec:m3b-confirmed`. Must confirm: `j_inode_val` xfields offset is 92; xfields walk advances `round_up(x_size, 8)` per field; DSTREAM (type 8) value parsed as `j_dstream_t` (size @0); FILE_EXTENT records keyed by `private_id` (NOT inode number); extents ordered by `logical_addr` and output truncated to `dstream.size`; no `unwrap`/`expect`/`panic`/raw indexing outside `#[cfg(test)]`; `#![forbid(unsafe_code)]` intact; decisive test passes un-skipped; decmpfs/xattr not implemented (correctly deferred). Fix any finding, re-review until clean.

- [ ] **Step 3: Finish the branch (squash + delete)**

Use the **superpowers:finishing-a-development-branch** skill. Choose "Merge back to main locally": squash-merge `feat/m3b-file-content` into `main`, re-run the gate on the merged result (expect apfs-core 31 tests green, clippy/fmt clean), delete the branch (linear history per CLAUDE.md).

- [ ] **Step 4: Update ROADMAP + re-index KB**

Edit `docs/ROADMAP.md`: add a status-table row `| M3b file content read (inode+dstream+file extents) | ✅ | <squash commit> |`; update the "Okuma yolu" paragraph (now reads file bytes; test count 31); in "Kalan yol haritası" mark M3b done and set the remaining M3 work = **M3c** (decmpfs ZLIB/LZVN/LZFSE + `com.apple.decmpfs` xattr + xattr/Finder metadata); update "Compact sonrası DEVAM" step 4 to point to M3c. Commit:

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add docs/ROADMAP.md
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "docs: ROADMAP — M3b file content read merged; next = M3c decmpfs+xattr

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

Then re-index: `ctx_index(source:"project:roadmap-state", path:"/Users/eiaydin/Documents/projects/apfs-explorer/docs/ROADMAP.md")`.

---

## Self-Review (performed while writing this plan)

**1. Spec coverage** (vs `apfs-spec:m3b-confirmed`): packed j_inode_val prefix + xfields @92 → Task 1 `Inode::parse` (`INODE_XFIELDS_OFFSET=92`). x_field_t 4B + `round_up(x_size,8)` walk → Task 1 (`round_up8`, the `dstream_after_unaligned_first_field_uses_round_up_8` test proves the rule). j_dstream_t 5×u64, size @0 → Task 1 `JDstream`. INO_EXT_TYPE_DSTREAM=8 → Task 1. FILE_EXTENT keyed by `private_id`, key logical_addr @8, val len_and_flags&0x00ff..@0 / phys_block_num @8 → Task 2 `read_file`. Assemble by logical_addr + truncate to dstream.size → Task 2. Real-image proof + independent Python + Fletcher-64 → Task 2 decisive test + Task 3 Python. decmpfs/xattr explicitly out of scope. All covered.

**2. Placeholder scan:** no TBD/TODO/"handle edge cases"; every code step is complete; commands exact with expected output; no "similar to Task N".

**3. Type consistency:** `Inode{parent_id,private_id,internal_flags,dstream:Option<JDstream>}`, `JDstream{size,alloced_size,default_crypto_id,total_bytes_written,total_bytes_read}`, `JDstream::parse`, `Inode::parse`, `INO_EXT_TYPE_DSTREAM`, `INODE_XFIELDS_OFFSET`, `Catalog::read_file<D:BlockDevice>(&self,&mut D,u64)->Result<Vec<u8>,ContainerError>` consistent across tasks and with the existing M3a API (`for_each_leaf` private `&mut dyn FnMut`, `oor()`, `self.root`/`self.block_size`, `JKey`, `APFS_TYPE_INODE`/`APFS_TYPE_FILE_EXTENT`, `ROOT_DIR_INO_NUM`, `FileBlockDevice`, `Container`, `vol.names_are_hashed()`, `DirEntry.file_id`). lib.rs re-export added once after the catalog re-export.
