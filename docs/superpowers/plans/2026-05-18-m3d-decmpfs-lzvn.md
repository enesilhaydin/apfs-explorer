# M3d — decmpfs Framework + LZVN Decompression Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`). **Lean consolidated process (CLAUDE.md "Süreç (konsolide)"):** per task = implementer (TDD red→green, self-test once) + ONE consolidated independent review (spec + quality + gates; reviewer reads the diff and runs gates, does not trust the implementer report). Milestone end = 1 final review + 1 decisive Python cross-check.

**Goal:** Transparently decompress macOS `com.apple.decmpfs` LZVN-compressed files (method 7 in-xattr, method 8 64k-chunked resource fork), proven on a GENUINE macOS LZVN system file copied into the real `apfs-large.img` fixture and validated byte-exact against the macOS kernel's own decompression output.

**Architecture:** Pure-Rust, `#![forbid(unsafe_code)]`. New `lzvn.rs` = a faithful, bounds-checked safe-Rust port of Apple's authoritative `lzvn_decode_base.c` (indexed KB source `apfs-spec:lzvn-ref`). New `decmpfs.rs` = parse the 16-byte `fpmc` header, dispatch by compression method (1 raw / 7 LZVN-xattr / 8 LZVN-resource-fork; 3/4/11/12 → explicit `Unsupported`). `catalog.rs` gains `read_file_decompressed` (if a file has a `com.apple.decmpfs` xattr → run decmpfs; else fall back to the M3b `read_file`). `xtask` copies a real small `/usr/bin` method-8 file's on-disk `com.apple.decmpfs` + `com.apple.ResourceFork` xattrs onto a fixture file plus the kernel-decompressed expected bytes, so the decisive test runs real macOS LZVN bytes through our decoder and compares to macOS's own output.

**Tech Stack:** Rust (`apfs-core`, `xtask`), `thiserror`, macOS `hdiutil` + Python 3 `ctypes` (built-in) for the fixture xattr copy, Python 3 stdlib for the independent cross-check.

**Spec sources (locked; query before coding):** `ctx_search(source:"apfs-spec:m3d-confirmed")` (decmpfs header, methods, method-7/8 layout — method-8 offset-array layout is REAL-DATA-confirmed, libfsapfs left it "TODO"), `ctx_search(source:"apfs-spec:lzvn-ref")` (Apple authoritative `lzvn_decode_base.c` — the LZVN algorithm; port faithfully), `apfs-spec:libfsapfs`, `apfs-spec:m3c-confirmed` (xattr reading via `Catalog::get_xattr`).

**Out of scope (do NOT implement — later milestones):** ZLIB methods 1-is-in-scope but 3/4 = M3e; LZFSE methods 11/12 = M3f; decmpfs method 5 (sparse), 6/9/10, 0x80000001. For unsupported methods return a clear `DecmpfsError::UnsupportedMethod(u32)` — never panic, never guess.

**Mandatory rules:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (no `cd`); NEVER `git add -A`/`git add .` — explicit filenames; English code/comments/commits; `apfs-core` stays `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic!`/raw indexing in production paths (test modules use `#[cfg(test)]` + the established `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]` once between `#[cfg(test)]` and `mod tests {` — check the target file's test module; add scoped-to-test only if absent, never duplicated/leaked); clippy `--all-targets --all-features -- -D warnings` green; commit trailer (exact last line) `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m3d-decmpfs-lzvn`.

**Gate command:**
```
cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace && \
cargo clippy --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace --all-targets --all-features -- -D warnings && \
cargo fmt --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --all --check
```

---

## File Structure

- **Create** `crates/apfs-core/src/lzvn.rs` — `pub fn lzvn_decode(src: &[u8], expected_len: usize) -> Result<Vec<u8>, ParseError>`. A faithful safe port of Apple `lzvn_decode_base.c` as a bounds-checked state machine (no `unsafe`, no panic). Unit tests: a real-macOS-derived single-chunk vector + synthetic eos/literal.
- **Create** `crates/apfs-core/src/decmpfs.rs` — `DecmpfsHeader` (parse 16B), `DecmpfsError`, `pub fn decompress(decmpfs_xattr: &[u8], resource_fork: Option<&[u8]>) -> Result<Vec<u8>, DecmpfsError>` dispatching methods 1/7/8 (others → `UnsupportedMethod`).
- **Modify** `crates/apfs-core/src/catalog.rs` — add `pub fn read_file_decompressed<D: BlockDevice>(&self, dev: &mut D, inode_num: u64) -> Result<Vec<u8>, ContainerError>`: if the inode has a `com.apple.decmpfs` xattr → fetch it (+ `com.apple.ResourceFork` if present) and `decmpfs::decompress`; else delegate to `read_file`. Add the decisive real-macOS test.
- **Modify** `crates/apfs-core/src/lib.rs` — `pub mod lzvn; pub use lzvn::lzvn_decode;` and `pub mod decmpfs; pub use decmpfs::{decompress as decmpfs_decompress, DecmpfsError, DecmpfsHeader};`.
- **Modify** `xtask/src/main.rs` — in `gen_large_fixture`, after the existing populate/xattr block and before `hdiutil detach`, run an embedded Python helper (via `std::process::Command::new("python3").arg("-c")`) that: finds the first small genuine `/usr/bin` method-8 file, reads its `com.apple.decmpfs` + `com.apple.ResourceFork` (libc `getxattr` with `XATTR_SHOWCOMPRESSION = 0x0020`), reads its kernel-decompressed content (normal `open`), then on the mounted image writes `dir00/cmp_lzvn.bin` (a 1-byte placeholder file) with those two xattrs (libc `setxattr`), and writes `dir00/cmp_lzvn.expected` as a normal file containing the kernel-decompressed bytes. Best-effort: if no readable method-8 source exists, skip silently (the decisive test then skips).

Existing API reused (do NOT change behavior): `Catalog::read_file` / `get_xattr` / `list_xattrs` / `list_dir` (M3a–c), `DirEntry{name,file_id,flags}`, `crate::endian::{u16_le,u32_le,u64_le,ParseError}`, `crate::jkey::ROOT_DIR_INO_NUM`, `Container`, `FileBlockDevice`, `vol.names_are_hashed()`.

---

## Task 1: lzvn.rs — faithful safe-Rust port of Apple LZVN

**Files:**
- Create: `crates/apfs-core/src/lzvn.rs`
- Modify: `crates/apfs-core/src/lib.rs`

Authoritative algorithm source: `ctx_search(source:"apfs-spec:lzvn-ref", queries:[...])` — this is Apple's own `lzvn_decode_base.c` (BSD-licensed), fully indexed. Port it FAITHFULLY. The C uses computed-goto (labels-as-values) + raw pointers; implement the exact same opcode semantics as a `loop`/`match` state machine over `src: &[u8]` writing into `out: Vec<u8>`, with every read/write bounds-checked (`.get(..)`, `checked_*`) — return `Err(ParseError::Short{..})` on truncation/invalid opcode, never panic.

**Opcode model (from `apfs-spec:lzvn-ref`; the implementer MUST query that source to confirm every exact bit-field before coding):**
- State: `src` cursor, `out` (push-only), `d_prev: usize` (last match distance, preserved across opcodes).
- 256-entry opcode dispatch on the first byte `opc` (the jump table in `apfs-spec:lzvn-ref`): values map to `sml_d`, `lrg_d`, `med_d`, `pre_d`, `sml_m`, `lrg_m`, `sml_l`, `lrg_l`, `eos`, `nop`, `udef`.
- `extract(opc_or_word, lo_bit, nbits)` = `(value >> lo_bit) & ((1<<nbits)-1)`.
- `sml_d` (LLMMMDDD DDDDDDDD LIT): opc_len=2; `L=extract(opc,6,2)`; `M=extract(opc,3,3)+3`; `D=(extract(opc,0,3)<<8) | src[1]`; → copy_literal_and_match.
- `lrg_d` (LLMMM111 DD DD LIT): opc_len=3; `L=extract(opc,6,2)`; `M=extract(opc,3,3)+3`; `D=u16_le(src,1)`; → copy_literal_and_match.
- `med_d` (101LLMMM DDDDDDMM DDDDDDDD LIT) opcodes `0xA0..=0xBF`: opc_len=3; `L=extract(opc,3,2)`; `word=u16_le(src,1)`; `M=(extract(opc,0,3)<<2 | extract(word,0,2))+3`; `D=word>>2`; → copy_literal_and_match. **Confirm this med_d bit layout verbatim against `apfs-spec:lzvn-ref` before coding.**
- `pre_d` (LLMMM110 LIT): opc_len=1; `L=extract(opc,6,2)`; `M=extract(opc,3,3)+3`; `D=d_prev` (unchanged); → copy_literal_and_match.
- `sml_m` (1111MMMM): opc_len=1; `M=extract(opc,0,4)`; → copy_match.
- `lrg_m` (11110000 MMMMMMMM): opc_len=2; `M=src[1]+16`; → copy_match.
- `sml_l` (1110LLLL): opc_len=1; `L=extract(opc,0,4)`; → copy_literal.
- `lrg_l` (11100000 LLLLLLLL): opc_len=2; `L=src[1]+16`; → copy_literal.
- `eos` (opcode `0x06`, total 8 bytes `06 00 00 00 00 00 00 00`): stop, success.
- `nop` (`0x0E`): opc_len=1, continue. `udef`: return `Err`.
- `copy_literal_and_match`: advance src past `opc_len`; copy `L` literal bytes `src→out` (bounds-checked, advance src by L); then `copy_match`.
- `copy_match`: require `D >= 1` and `D <= out.len()`; copy `M` bytes one-at-a-time from `out[out.len()-D]` appending to `out` (overlap-safe byte copy — do NOT use copy_from_slice for overlapping ranges); set `d_prev=D`.
- `copy_literal` (no match): advance src past `opc_len`; copy `L` bytes `src→out`; `d_prev` unchanged.
- Stop when `out.len() == expected_len` (return `out`) or `eos`. If src exhausted before reaching `expected_len` without `eos`, or any bounds check fails → `Err(ParseError::Short{at:0,need:expected_len,len:out.len()})`.

- [ ] **Step 1: Create the module with failing tests**

Create `crates/apfs-core/src/lzvn.rs`. Include the public function, the state machine per the model above (query `apfs-spec:lzvn-ref` for exact byte encodings — it is the authoritative reference, treat any divergence from it as a bug), and this test module:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    // Literal-only then eos: sml_l (1110LLLL) L=5 "hello", then eos (0x06 + 7 zero bytes).
    #[test]
    fn literal_then_eos() {
        let mut s = vec![0xE5u8]; // 1110_0101 -> sml_l L=5
        s.extend_from_slice(b"hello");
        s.push(0x06);
        s.extend_from_slice(&[0u8; 7]);
        let out = lzvn_decode(&s, 5).unwrap();
        assert_eq!(out, b"hello");
    }

    // Back-reference: literal "ab" then a match copying 4 bytes at distance 2 => "ababab".
    // Encoded with sml_d: opc = LLMMMDDD with L=2,M=(1)+3=4,D=2 -> bits LL=10 MMM=001 DDD=010
    // = 1000_1010 = 0x8A ; second byte = D low 8 bits = 0x02 ; then 2 literal bytes "ab".
    #[test]
    fn small_distance_backref() {
        let mut s = vec![0x8A, 0x02, b'a', b'b'];
        s.push(0x06);
        s.extend_from_slice(&[0u8; 7]);
        let out = lzvn_decode(&s, 6).unwrap();
        assert_eq!(out, b"ababab");
    }

    #[test]
    fn truncated_src_errors_not_panics() {
        assert!(lzvn_decode(&[0xE5, b'h'], 5).is_err()); // claims 5 literals, only 1 present
        assert!(lzvn_decode(&[], 0).is_ok()); // empty -> empty (expected_len 0)
    }
}
```

(Note: `small_distance_backref`'s exact opcode bytes must be re-derived by the implementer from the `apfs-spec:lzvn-ref` `sml_d` bit layout — if the captured layout above is off, fix the TEST INPUT bytes to match the authoritative encoding, NOT the decoder, and add a comment showing the bit derivation. The decisive proof of correctness is Task 2's real-macOS test, not these synthetic vectors.)

Add to `crates/apfs-core/src/lib.rs` immediately after the line `pub use xattr::{XattrDstream, XattrEntry};`:

```rust

pub mod lzvn;
pub use lzvn::lzvn_decode;
```

- [ ] **Step 2: Red → green**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core lzvn`
Expected: compile-fail until wired, then the 3 unit tests pass. (If a synthetic vector's bytes were mis-derived, correct the TEST bytes per `apfs-spec:lzvn-ref` — never weaken the decoder to match a wrong vector.)

- [ ] **Step 3: Full gate** — run the **Gate command**; all green. apfs-core test count = previous 36 + 3 = 39.

- [ ] **Step 4: Commit**
```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/lzvn.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3d lzvn — faithful safe-Rust port of Apple LZVN decoder

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: decmpfs.rs + catalog integration + xtask real-macOS fixture + decisive test

**Files:**
- Create: `crates/apfs-core/src/decmpfs.rs`
- Modify: `crates/apfs-core/src/lib.rs`, `crates/apfs-core/src/catalog.rs`, `xtask/src/main.rs`

- [ ] **Step 1: Create `crates/apfs-core/src/decmpfs.rs`**

```rust
//! com.apple.decmpfs transparent decompression. Header + method dispatch.
//! Spec: ctx_search(source:"apfs-spec:m3d-confirmed"). Method-8 resource-fork
//! offset-array layout was reverse-engineered from real macOS (libfsapfs
//! leaves it "TODO"); the decisive arbiter is the real-image test.
use crate::endian::{u32_le, u64_le, ParseError};
use crate::lzvn::lzvn_decode;

const DECMPFS_MAGIC: u32 = 0x636d_7066; // bytes 'f','p','m','c' as LE u32 ("cmpf")
const CHUNK: usize = 65536;

#[derive(Debug, PartialEq, Eq)]
pub enum DecmpfsError {
    Parse(ParseError),
    BadMagic(u32),
    UnsupportedMethod(u32),
    MissingResourceFork,
    LengthMismatch { got: usize, want: usize },
}
impl From<ParseError> for DecmpfsError {
    fn from(e: ParseError) -> Self {
        DecmpfsError::Parse(e)
    }
}

/// Parsed 16-byte com.apple.decmpfs header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecmpfsHeader {
    pub method: u32,
    pub uncompressed_size: u64,
}

impl DecmpfsHeader {
    pub fn parse(x: &[u8]) -> Result<Self, DecmpfsError> {
        let magic = u32_le(x, 0)?;
        if magic != DECMPFS_MAGIC {
            return Err(DecmpfsError::BadMagic(magic));
        }
        Ok(Self {
            method: u32_le(x, 4)?,
            uncompressed_size: u64_le(x, 8)?,
        })
    }
}

/// Decompress a decmpfs-compressed file. `decmpfs_xattr` = the full
/// com.apple.decmpfs value (incl. 16-byte header). `resource_fork` = the
/// com.apple.ResourceFork value if present (required for method 8).
pub fn decompress(
    decmpfs_xattr: &[u8],
    resource_fork: Option<&[u8]>,
) -> Result<Vec<u8>, DecmpfsError> {
    let h = DecmpfsHeader::parse(decmpfs_xattr)?;
    let usize_total = h.uncompressed_size as usize;
    let payload = decmpfs_xattr.get(16..).ok_or(ParseError::Short {
        at: 16,
        need: 0,
        len: decmpfs_xattr.len(),
    })?;
    let out = match h.method {
        1 => payload
            .get(..usize_total)
            .ok_or(ParseError::Short {
                at: 0,
                need: usize_total,
                len: payload.len(),
            })?
            .to_vec(),
        7 => {
            // LZVN in the decmpfs xattr; 0xFF first byte = stored raw.
            if payload.first() == Some(&0xFF) {
                payload
                    .get(1..1 + usize_total)
                    .ok_or(ParseError::Short {
                        at: 1,
                        need: usize_total,
                        len: payload.len(),
                    })?
                    .to_vec()
            } else {
                lzvn_decode(payload, usize_total)?
            }
        }
        8 => {
            let rf = resource_fork.ok_or(DecmpfsError::MissingResourceFork)?;
            let n = usize_total.div_ceil(CHUNK);
            // (n+1) u32 LE offsets relative to rf start; block i = rf[off[i]..off[i+1]].
            let mut offs = Vec::with_capacity(n + 1);
            for i in 0..=n {
                offs.push(u32_le(rf, 4 * i)? as usize);
            }
            let mut out = Vec::with_capacity(usize_total);
            for i in 0..n {
                let (a, b) = (offs[i], offs[i + 1]);
                let blk = rf.get(a..b).ok_or(ParseError::Short {
                    at: a,
                    need: b.saturating_sub(a),
                    len: rf.len(),
                })?;
                let want = core::cmp::min(CHUNK, usize_total - i * CHUNK);
                // A chunk stored uncompressed is prefixed with 0x06 (decmpfs).
                if blk.first() == Some(&0x06) && blk.len() == want + 1 {
                    out.extend_from_slice(blk.get(1..).unwrap_or(&[]));
                } else {
                    out.extend_from_slice(&lzvn_decode(blk, want)?);
                }
            }
            out
        }
        m => return Err(DecmpfsError::UnsupportedMethod(m)),
    };
    if out.len() != usize_total {
        return Err(DecmpfsError::LengthMismatch {
            got: out.len(),
            want: usize_total,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn method1_raw_roundtrip() {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63]; // 'fpmc'
        x.extend_from_slice(&1u32.to_le_bytes());
        x.extend_from_slice(&5u64.to_le_bytes());
        x.extend_from_slice(b"hello");
        assert_eq!(decompress(&x, None).unwrap(), b"hello");
    }

    #[test]
    fn bad_magic_and_unsupported_error_not_panic() {
        assert!(matches!(
            decompress(&[0u8; 16], None),
            Err(DecmpfsError::BadMagic(_))
        ));
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&11u32.to_le_bytes()); // LZFSE -> M3f, unsupported here
        x.extend_from_slice(&0u64.to_le_bytes());
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::UnsupportedMethod(11))
        ));
    }

    #[test]
    fn method8_missing_rsrc_errors() {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&8u32.to_le_bytes());
        x.extend_from_slice(&100u64.to_le_bytes());
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::MissingResourceFork)
        ));
    }
}
```

Add to `crates/apfs-core/src/lib.rs` immediately after the `pub use lzvn::lzvn_decode;` line from Task 1:

```rust

pub mod decmpfs;
pub use decmpfs::{decompress as decmpfs_decompress, DecmpfsError, DecmpfsHeader};
```

- [ ] **Step 2: Add `read_file_decompressed` to `catalog.rs`**

In `crates/apfs-core/src/catalog.rs`, inside `impl Catalog`, directly after `get_xattr`, add:

```rust
    /// Read a file's logical content, transparently decompressing
    /// com.apple.decmpfs files (LZVN methods 1/7/8 — M3d). Non-compressed
    /// files fall back to `read_file`.
    pub fn read_file_decompressed<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<u8>, ContainerError> {
        let dec = self.get_xattr(dev, inode_num, "com.apple.decmpfs")?;
        match dec {
            None => self.read_file(dev, inode_num),
            Some(d) => {
                let rf = self.get_xattr(dev, inode_num, "com.apple.ResourceFork")?;
                crate::decmpfs::decompress(&d, rf.as_deref()).map_err(|e| {
                    ContainerError::Parse(crate::endian::ParseError::Short {
                        at: 0,
                        need: 0,
                        len: format!("{e:?}").len(),
                    })
                })
            }
        }
    }
```

- [ ] **Step 3: Extend the fixture generator (`xtask/src/main.rs`)**

In `fn gen_large_fixture`, immediately AFTER the M3c xattr block (`user.apfsx_small`/`user.apfsx_big` on `dir00/f000.bin`) and BEFORE the `hdiutil detach` command, insert:

```rust
    // M3d: embed a GENUINE macOS LZVN (decmpfs method 8) sample into the image
    // plus its kernel-decompressed expected content, so the decoder is proven
    // against real macOS bytes vs macOS's own output. Best-effort (macOS only).
    let mnt_str = mnt.to_str().unwrap_or("");
    let py = format!(
        r#"
import os,sys,struct,ctypes,ctypes.util,subprocess
M={mnt!r}
libc=ctypes.CDLL(ctypes.util.find_library("c"),use_errno=True)
libc.getxattr.restype=ctypes.c_ssize_t
libc.getxattr.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_void_p,ctypes.c_size_t,ctypes.c_uint32,ctypes.c_int]
libc.setxattr.restype=ctypes.c_int
libc.setxattr.argtypes=[ctypes.c_char_p,ctypes.c_char_p,ctypes.c_void_p,ctypes.c_size_t,ctypes.c_uint32,ctypes.c_int]
SHOW=0x0020
def gx(p,n):
    s=libc.getxattr(p.encode(),n.encode(),None,0,0,SHOW)
    if s<0: return None
    b=ctypes.create_string_buffer(s); libc.getxattr(p.encode(),n.encode(),b,s,0,SHOW); return b.raw[:s]
def sx(p,n,v):
    if libc.setxattr(p.encode(),n.encode(),v,len(v),0,0)!=0: raise OSError(ctypes.get_errno(),"setxattr "+n)
cands=subprocess.run(["bash","-lc","find /usr/bin /usr/lib -maxdepth 1 -type f -flags +compressed 2>/dev/null"],capture_output=True,text=True).stdout.split()
src=None
for p in cands:
    d=gx(p,"com.apple.decmpfs")
    if d and len(d)>=16 and struct.unpack_from('<I',d,4)[0]==8:
        u=struct.unpack_from('<Q',d,8)[0]
        if 0<u<=400000:
            r=gx(p,"com.apple.ResourceFork")
            if r: src=(p,d,r,u); break
if src is None:
    print("M3D_FIXTURE_SKIP no readable method-8 sample"); sys.exit(0)
p,d,r,u=src
content=open(p,'rb').read()           # kernel transparently decompresses == ORACLE
assert len(content)==u, (len(content),u)
bn=os.path.join(M,"dir00","cmp_lzvn.bin")
open(bn,'wb').write(b"\x00")          # create; data fork irrelevant (decmpfs xattr drives it)
sx(bn,"com.apple.decmpfs",d); sx(bn,"com.apple.ResourceFork",r)
open(os.path.join(M,"dir00","cmp_lzvn.expected"),'wb').write(content)
print("M3D_FIXTURE_OK src=%s usize=%d"%(os.path.basename(p),u))
"#,
        mnt = mnt_str
    );
    let _ = std::process::Command::new("python3").arg("-c").arg(&py).status();
```

(If `mnt` is not already a `PathBuf`/`Path` in scope with `.to_str()`, use the existing mount-point variable used by the M3c block — match the surrounding code exactly.)

- [ ] **Step 4: Regenerate the fixture**

Run: `cargo run --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p xtask -- gen-fixture --large`
Expected: prints `M3D_FIXTURE_OK src=... usize=...` (or `M3D_FIXTURE_SKIP ...` if no readable sample — then the decisive test will skip; on this macOS `/usr/bin` has ~149 method-8 files so OK is expected). `dir00/cmp_lzvn.bin` (real macOS decmpfs+ResourceFork xattrs) and `dir00/cmp_lzvn.expected` (kernel-decompressed bytes) now exist in `apfs-large.img`.

- [ ] **Step 5: Add the decisive real-macOS test**

In the existing `#[cfg(test)] mod tests` block in `crates/apfs-core/src/catalog.rs`, add:

```rust
    #[test]
    fn decompresses_real_macos_lzvn_file() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !std::path::Path::new(img).exists() {
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

        let root = cat.list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed).expect("root");
        let dir00 = match root.iter().find(|e| e.name == "dir00") {
            Some(d) => d.clone(),
            None => {
                eprintln!("skip: size-only fixture — regenerate gen-fixture --large");
                return;
            }
        };
        let kids = cat.list_dir(&mut dev, dir00.file_id, hashed).expect("dir00");
        let bin = match kids.iter().find(|e| e.name == "cmp_lzvn.bin") {
            Some(e) => e.clone(),
            None => {
                eprintln!(
                    "skip: fixture lacks M3d LZVN sample (M3D_FIXTURE_SKIP at gen time) — \
                     re-run gen-fixture --large on a macOS host with readable /usr/bin"
                );
                return;
            }
        };
        let exp = kids
            .iter()
            .find(|e| e.name == "cmp_lzvn.expected")
            .expect("cmp_lzvn.expected present");

        let got = cat
            .read_file_decompressed(&mut dev, bin.file_id)
            .expect("decmpfs-decompress real macOS LZVN file");
        let want = cat
            .read_file(&mut dev, exp.file_id)
            .expect("read expected content");
        assert_eq!(
            got.len(),
            want.len(),
            "decompressed length must match macOS kernel output"
        );
        assert!(
            got == want,
            "LZVN-decompressed bytes must equal macOS kernel's own decompression"
        );

        // Non-compressed regression: f000.bin still reads via the fallback path.
        let f000 = kids.iter().find(|e| e.name == "f000.bin").expect("f000.bin");
        let plain = cat
            .read_file_decompressed(&mut dev, f000.file_id)
            .expect("read plain via decompressed path");
        assert_eq!(plain.len(), 4096);
        assert!(plain.iter().all(|&b| b == b'A'));
    }
```

- [ ] **Step 6: Red → green**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core decompresses_real_macos_lzvn_file -- --nocapture`
Expected: compile-fail until `read_file_decompressed`/`decmpfs` exist; then PASS **un-skipped** (the real macOS LZVN file decompresses byte-exact to the kernel's own output). Also run `cargo test ... -p apfs-core reads_real_xattrs_embedded_and_stream reads_real_file_content_4096_a lists_real_large_fixture_tree` — all still pass un-skipped (no M3a–c regression). If the assertion fails, the LZVN port (Task 1) has a defect — fix `lzvn.rs` against `apfs-spec:lzvn-ref`; do NOT weaken assertions.

- [ ] **Step 7: Full gate** — run the **Gate command**. apfs-core test count = 39 + 3 (decmpfs unit) + 1 (decisive) = 43. All green.

- [ ] **Step 8: Commit**
```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/decmpfs.rs crates/apfs-core/src/lib.rs crates/apfs-core/src/catalog.rs xtask/src/main.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3d decmpfs — header + method 1/7/8 dispatch + LZVN file read; real-macOS fixture

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Milestone end — independent Python cross-check + final review + merge

**Files:** none modified (verification + branch finish + ROADMAP).

- [ ] **Step 1: Independent Python cross-check (decisive)**

Run via `ctx_execute(language:"python", code: <below>)`. It shares NO code with Rust: it independently reverse-decodes the raw `apfs-large.img`, finds `dir00/cmp_lzvn.bin`, reads its on-disk `com.apple.decmpfs` + `com.apple.ResourceFork` j_xattr records, reconstructs the method-8 offset-array layout, LZVN-decodes each chunk **with an independent Python LZVN decoder** (ported separately from `apfs-spec:lzvn-ref`), concatenates, and asserts the result equals `dir00/cmp_lzvn.expected`'s bytes (read independently from the image), and recomputes Fletcher-64 of the cmp_lzvn.bin inode's leaf node. The implementer writes this Python decoder fresh from `apfs-spec:lzvn-ref` (NOT copied from the Rust). Expected single printed line: `PYTHON-CROSSCHECK OK | method=8 chunks=<n> usize=<u> bytes_match=True | inode-leaf Fletcher-64 verified`. Reuse the catalog/omap/xattr walk from the M3c cross-check script (`ctx_search(source:"project:roadmap-state")` / prior plans) for the APFS traversal; add the offset-array + LZVN-decode step. If Python disagrees with Rust, STOP and investigate before merge.

- [ ] **Step 2: Final consolidated milestone review**

Dispatch ONE independent reviewer over the whole `feat/m3d-decmpfs-lzvn` branch vs `main`. Reviewer reads the full diff, runs the **Gate command**, runs the Step 1 Python cross-check, and verifies against `apfs-spec:m3d-confirmed` + `apfs-spec:lzvn-ref`. Must confirm: LZVN port is faithful to the authoritative `lzvn-ref` (opcode-by-opcode), bounds-checked, no `unsafe`/`unwrap`/`expect`/`panic`/raw indexing in production; decmpfs header magic/methods correct; method-8 offset-array layout matches `apfs-spec:m3d-confirmed`; unsupported methods (3/4/11/12) return `UnsupportedMethod` not panic; `read_file_decompressed` falls back to `read_file` for non-compressed files (no M3a–c regression — `reads_real_xattrs_embedded_and_stream`/`reads_real_file_content_4096_a`/`lists_real_large_fixture_tree` green); the decisive test passes UN-SKIPPED and compares against the genuine macOS kernel output; xtask change is best-effort and does not regress other fixtures. Fix findings, re-review until clean.

- [ ] **Step 3: Finish the branch (squash + delete)**

Use **superpowers:finishing-a-development-branch** → "Merge back to main locally": squash-merge `feat/m3d-decmpfs-lzvn` into `main`, re-run the gate on the merged result (expect apfs-core 43 tests green, clippy/fmt clean), delete the branch.

- [ ] **Step 4: Update ROADMAP + re-index KB**

Edit `docs/ROADMAP.md`: add status-table row `| M3d decmpfs+LZVN (methods 1/7/8) | ✅ | <squash commit> |`; update the "Okuma yolu" paragraph (now transparently decompresses LZVN; test count 43); in "Kalan yol haritası" mark M3d done and set remaining = **M3e** (ZLIB methods 3/4 — pure-Rust DEFLATE/RFC1951) then **M3f** (LZFSE methods 11/12); update "Compact sonrası DEVAM" step 1 (M0–M3d merged) and step 4 (next = M3e). Commit:
```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add docs/ROADMAP.md
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "docs: ROADMAP — M3d decmpfs+LZVN merged; next = M3e ZLIB

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```
Then re-index: `ctx_index(source:"project:roadmap-state", path:"/Users/eiaydin/Documents/projects/apfs-explorer/docs/ROADMAP.md")`.

---

## Self-Review (performed while writing this plan)

**1. Spec coverage** (vs `apfs-spec:m3d-confirmed` + `apfs-spec:lzvn-ref`): decmpfs 16B header (fpmc magic 0x636d7066, method@4, usize@8) → Task 2 `DecmpfsHeader::parse`. Method 1 raw → Task 2 dispatch. Method 7 LZVN-xattr + 0xFF raw fallback → Task 2. Method 8 LZVN-rsrc real-data offset-array (N=ceil(usize/65536), (N+1) u32 LE offsets, block i=rf[off[i]..off[i+1]], 0x06 raw-chunk fallback) → Task 2. Unsupported 3/4/11/12 → `UnsupportedMethod`. LZVN algorithm faithful port of authoritative `apfs-spec:lzvn-ref` → Task 1. catalog integration + non-compressed fallback → Task 2 `read_file_decompressed`. Real-macOS proof (genuine /usr/bin method-8 sample vs kernel oracle) + independent Python + Fletcher-64 → Task 2 decisive test + Task 3.

**2. Placeholder scan:** decmpfs.rs / catalog / xtask code is complete and exact. `lzvn.rs` internals are a faithful port of the fully-indexed authoritative source `apfs-spec:lzvn-ref` with the opcode model spelled out and the decisive real-macOS oracle test as the correctness gate — this is a precise instruction to port a specific named authoritative reference (like "implement RFC 1951"), not a vague placeholder; the implementer queries `apfs-spec:lzvn-ref` for exact byte encodings (explicitly required). No "TBD"/"handle edge cases"/uncited types.

**3. Type consistency:** `lzvn_decode(&[u8], usize) -> Result<Vec<u8>, ParseError>`; `DecmpfsHeader{method:u32,uncompressed_size:u64}`, `DecmpfsHeader::parse -> Result<_,DecmpfsError>`, `decompress(&[u8], Option<&[u8]>) -> Result<Vec<u8>, DecmpfsError>`, `DecmpfsError` variants; `Catalog::read_file_decompressed<D:BlockDevice>(&self,&mut D,u64)->Result<Vec<u8>,ContainerError>` reusing M3c `get_xattr` + M3b `read_file`; lib.rs re-exports added once after the xattr re-export then after lzvn. `CHUNK=65536`, `div_ceil` (stable Rust). Consistent across tasks and with existing M3a–c API.
