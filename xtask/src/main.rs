//! xtask - fixture generation + differential harness (M2+). M1a: gen-fixture.
use std::path::Path;
use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gen-fixture") => {
            let large = args.iter().any(|a| a == "--large");
            let r = if large {
                gen_large_fixture()
            } else {
                gen_fixture()
            };
            match r {
                Ok(p) => println!("fixture ready: {p}"),
                Err(e) => {
                    eprintln!("gen-fixture failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("rt-snapshot") => {
            let mount = match args.get(1) {
                Some(m) => m.as_str(),
                None => {
                    eprintln!("usage: xtask gen-fixture [--large] | rt-snapshot <mount> | package");
                    std::process::exit(2);
                }
            };
            match rt_snapshot(mount) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("rt-snapshot failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("gen-scratch") => {
            let path = match args.get(1) {
                Some(p) => p.as_str(),
                None => {
                    eprintln!("usage: xtask gen-scratch <path> [size_mb]");
                    std::process::exit(2);
                }
            };
            let size_mb: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(64);
            match gen_scratch(path, size_mb) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("gen-scratch failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("verify-scratch") => {
            let path = match args.get(1) {
                Some(p) => p.as_str(),
                None => {
                    eprintln!("usage: xtask verify-scratch <path>");
                    std::process::exit(2);
                }
            };
            match verify_scratch(path) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("verify-scratch failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("package") => match package() {
            Ok(()) => {}
            Err(e) => {
                eprintln!("package failed: {e}");
                std::process::exit(1);
            }
        },
        Some("diff-scratch") => {
            let path = match args.get(1) {
                Some(p) => p.as_str(),
                None => {
                    eprintln!("usage: xtask diff-scratch <path>");
                    std::process::exit(2);
                }
            };
            match diff_scratch(path) {
                Ok(manifest) => print!("{manifest}"),
                Err(e) => {
                    eprintln!("diff-scratch failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("populate-scratch") => {
            let path = match args.get(1) {
                Some(p) => p.as_str(),
                None => {
                    eprintln!("usage: xtask populate-scratch <path> <count>");
                    std::process::exit(2);
                }
            };
            let count: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(250);
            match populate_scratch(path, count) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("populate-scratch failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("set-version") => {
            let ver = match args.get(1) {
                Some(v) => v.as_str(),
                None => {
                    eprintln!("usage: xtask set-version <X.Y.Z>");
                    std::process::exit(2);
                }
            };
            match set_version(ver) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("set-version failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!(
                "usage: xtask gen-fixture [--large] | rt-snapshot <mount> \
                 | gen-scratch <path> [size_mb] | verify-scratch <path> \
                 | populate-scratch <path> <count> \
                 | diff-scratch <path> | set-version <X.Y.Z> | package"
            );
            std::process::exit(2);
        }
    }
}

/// Bump the bundle version in `crates/apfs-explorer-gui/tauri.conf.json`.
///
/// The Tauri MSI's UpgradeCode is stable, so installing a build with a HIGHER
/// version makes `msiexec /i` perform a major upgrade (uninstall old, install
/// new) instead of a no-op same-version reinstall. Run this, then rebuild the
/// MSI (`cargo tauri build`) to ship an upgrade.
fn set_version(ver: &str) -> Result<String, String> {
    // Validate a simple dotted-numeric version, e.g. 1.0.1 or 1.2.3.4.
    let parts: Vec<&str> = ver.split('.').collect();
    let ok = (2..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        return Err(format!("invalid version {ver:?}; expected e.g. 1.0.1"));
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../crates/apfs-explorer-gui/tauri.conf.json"
    );
    let content = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let key = "\"version\":";
    let kstart = content
        .find(key)
        .ok_or_else(|| "no \"version\" field in tauri.conf.json".to_string())?;
    let after = &content[kstart + key.len()..];
    let q1 = after
        .find('"')
        .ok_or_else(|| "malformed version field".to_string())?;
    let val_start = kstart + key.len() + q1 + 1;
    let q2 = content[val_start..]
        .find('"')
        .ok_or_else(|| "malformed version field".to_string())?;
    let val_end = val_start + q2;
    let old = content[val_start..val_end].to_string();
    if old == ver {
        return Ok(format!("version already {ver}; no change"));
    }
    let mut out = String::with_capacity(content.len());
    out.push_str(&content[..val_start]);
    out.push_str(ver);
    out.push_str(&content[val_end..]);
    std::fs::write(path, out).map_err(|e| format!("write {path}: {e}"))?;
    Ok(format!(
        "version {old} -> {ver} in tauri.conf.json (rebuild the MSI to ship the upgrade)"
    ))
}

/// Differential oracle: attach a scratch image READ-ONLY via macOS (Apple's own
/// APFS driver - the independent authority), walk the mounted volume, and emit a
/// manifest `relpath\tsize\tsha256` per regular file. A test compares this
/// against the content it wrote via OUR stack: if macOS reads back byte-for-byte
/// what we wrote, the write path is content-correct (beyond fsck's structural
/// check). macOS-only.
fn diff_scratch(path: &str) -> Result<String, String> {
    let att = Command::new("hdiutil")
        .args(["attach", "-readonly", path])
        .output()
        .map_err(|e| format!("hdiutil attach spawn: {e}"))?;
    if !att.status.success() {
        return Err(format!(
            "hdiutil attach: {}",
            String::from_utf8_lossy(&att.stderr)
        ));
    }
    let att_text = String::from_utf8_lossy(&att.stdout);
    let mut parent_dev: Option<String> = None;
    let mut mountpoint: Option<String> = None;
    for line in att_text.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').map(str::trim).collect();
        let (Some(dev), Some(kind)) = (parts.first(), parts.get(1)) else {
            continue;
        };
        if kind.contains("GUID_partition_scheme") && parent_dev.is_none() {
            parent_dev = Some((*dev).to_string());
        } else if kind.starts_with("41504653") {
            if let Some(mnt) = parts.get(2) {
                if !mnt.is_empty() {
                    mountpoint = Some((*mnt).to_string());
                }
            }
        }
    }
    let parent_dev =
        parent_dev.ok_or_else(|| format!("no GUID_partition_scheme device:\n{att_text}"))?;
    let mountpoint = match mountpoint {
        Some(m) => m,
        None => {
            let _ = Command::new("hdiutil")
                .args(["detach", &parent_dev])
                .output();
            return Err(format!("no APFS volume mountpoint:\n{att_text}"));
        }
    };
    let mut out = String::new();
    let root = Path::new(&mountpoint);
    let walk_res = walk_manifest(root, root, &mut out);
    // Always detach, even on walk error.
    let _ = Command::new("hdiutil")
        .args(["detach", &parent_dev])
        .output();
    walk_res?;
    Ok(out)
}

/// Populate a scratch image with `count` files written by macOS's OWN APFS
/// driver (attach READ-WRITE, write via std::fs, detach). The resulting catalog
/// B-tree is laid out exactly as Apple lays it out - different leaf split points,
/// TOC ordering and packing than our `rewrite_fstree`. A subsequent mutation
/// workload then exercises OUR write path against an Apple-authored tree, which
/// is what the real-USB #156 corruption needed (our scratch self-built trees
/// round-trip our own packer and never reproduced it). macOS-only.
fn populate_scratch(path: &str, count: usize) -> Result<String, String> {
    let att = Command::new("hdiutil")
        .args(["attach", path])
        .output()
        .map_err(|e| format!("hdiutil attach spawn: {e}"))?;
    if !att.status.success() {
        return Err(format!(
            "hdiutil attach: {}",
            String::from_utf8_lossy(&att.stderr)
        ));
    }
    let att_text = String::from_utf8_lossy(&att.stdout);
    let mut parent_dev: Option<String> = None;
    let mut mountpoint: Option<String> = None;
    for line in att_text.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').map(str::trim).collect();
        let (Some(dev), Some(kind)) = (parts.first(), parts.get(1)) else {
            continue;
        };
        if kind.contains("GUID_partition_scheme") && parent_dev.is_none() {
            parent_dev = Some((*dev).to_string());
        } else if kind.starts_with("41504653") {
            if let Some(mnt) = parts.get(2) {
                if !mnt.is_empty() {
                    mountpoint = Some((*mnt).to_string());
                }
            }
        }
    }
    let parent_dev =
        parent_dev.ok_or_else(|| format!("no GUID_partition_scheme device:\n{att_text}"))?;
    let mountpoint = match mountpoint {
        Some(m) => m,
        None => {
            let _ = Command::new("hdiutil")
                .args(["detach", &parent_dev])
                .output();
            return Err(format!("no APFS volume mountpoint:\n{att_text}"));
        }
    };
    // Write `count` files via macOS. Mirror the real-USB seal: `u*.txt` names
    // plus a few mixed extensions, small varied bodies.
    let write_res = (|| -> Result<(), String> {
        let root = Path::new(&mountpoint);
        for i in 0..count {
            let ext = match i % 5 {
                0 => "txt",
                1 => "dat",
                2 => "log",
                3 => "md",
                _ => "bin",
            };
            let name = format!("u{i:03}.{ext}");
            let body = format!("usb content {i}\n");
            std::fs::write(root.join(&name), body.as_bytes())
                .map_err(|e| format!("write {name}: {e}"))?;
        }
        Ok(())
    })();
    // Always detach (macOS flushes the volume on detach), even on write error.
    let _ = Command::new("hdiutil")
        .args(["detach", &parent_dev])
        .output();
    write_res?;
    Ok(format!(
        "populated {path} with {count} macOS-authored files (Apple-laid-out catalog)"
    ))
}

/// Recursively emit `relpath\tsize\tsha256` for each regular file under `dir`,
/// relative to `root`. Skips macOS-injected system entries.
fn walk_manifest(root: &Path, dir: &Path, out: &mut String) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".Spotlight") || name == ".fseventsd" || name == ".Trashes" {
            continue;
        }
        let p = entry.path();
        let ft = entry.file_type().map_err(|e| format!("file_type: {e}"))?;
        if ft.is_dir() {
            walk_manifest(root, &p, out)?;
        } else if ft.is_file() {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let sh = Command::new("shasum")
                .args(["-a", "256"])
                .arg(&p)
                .output()
                .map_err(|e| format!("shasum {}: {e}", p.display()))?;
            let hash = String::from_utf8_lossy(&sh.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .to_string();
            out.push_str(&format!("{rel}\t{size}\t{hash}\n"));
        }
    }
    Ok(())
}

/// Create a small real APFS container image at tests/fixtures/apfs-tiny.img.
/// macOS only (uses hdiutil). `-layout NONE` => no partition map, so the APFS
/// container superblock is at file offset 0 (physical block 0).
fn gen_fixture() -> Result<String, String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let proj = Path::new(root).parent().ok_or("no parent")?;
    let fixtures = proj.join("tests/fixtures");
    std::fs::create_dir_all(&fixtures).map_err(|e| e.to_string())?;
    let img = fixtures.join("apfs-tiny.img");
    if img.exists() {
        std::fs::remove_file(&img).map_err(|e| e.to_string())?;
    }
    let stem = fixtures.join("apfs-tiny");
    let out = Command::new("hdiutil")
        .args([
            "create",
            "-megabytes",
            "16",
            "-layout",
            "NONE",
            "-fs",
            "APFS",
            "-volname",
            "APFSTEST",
            "-ov",
            stem.to_str().ok_or("bad path")?,
        ])
        .output()
        .map_err(|e| format!("hdiutil spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!("hdiutil: {}", String::from_utf8_lossy(&out.stderr)));
    }
    let dmg = fixtures.join("apfs-tiny.dmg");
    std::fs::rename(&dmg, &img).map_err(|e| e.to_string())?;
    verify_nxsb(&img)
}

/// Create a larger (~256 MB) APFS image populated with a nested directory
/// tree so the object map / catalog B-trees gain multi-node depth. macOS only.
fn gen_large_fixture() -> Result<String, String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let proj = std::path::Path::new(root).parent().ok_or("no parent")?;
    let fixtures = proj.join("tests/fixtures");
    std::fs::create_dir_all(&fixtures).map_err(|e| e.to_string())?;
    let img = fixtures.join("apfs-large.img");
    if img.exists() {
        std::fs::remove_file(&img).map_err(|e| e.to_string())?;
    }
    let stem = fixtures.join("apfs-large");
    let out = std::process::Command::new("hdiutil")
        .args([
            "create",
            "-megabytes",
            "256",
            "-layout",
            "NONE",
            "-fs",
            "APFS",
            "-volname",
            "APFSLARGE",
            "-ov",
            stem.to_str().ok_or("bad path")?,
        ])
        .output()
        .map_err(|e| format!("hdiutil create: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "hdiutil create: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    std::fs::rename(fixtures.join("apfs-large.dmg"), &img).map_err(|e| e.to_string())?;
    // Attach the raw container image and populate it with a nested tree.
    let mnt = fixtures.join(".apfs-large-mnt");
    std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
    let att = std::process::Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-mountpoint",
            mnt.to_str().ok_or("bad mnt")?,
            img.to_str().ok_or("bad img")?,
        ])
        .output()
        .map_err(|e| format!("hdiutil attach: {e}"))?;
    if !att.status.success() {
        // Populate is best-effort; a size-only large image is still useful.
        eprintln!(
            "hdiutil attach fell back (size-only image): {}",
            String::from_utf8_lossy(&att.stderr)
        );
        let _ = std::fs::remove_dir_all(&mnt);
        return verify_nxsb(&img);
    }
    for d in 0..16 {
        let dir = mnt.join(format!("dir{d:02}"));
        let _ = std::fs::create_dir_all(&dir);
        for f in 0..64 {
            let _ = std::fs::write(dir.join(format!("f{f:03}.bin")), vec![b'A'; 4096]);
        }
    }
    // M3c: set a small (embedded) and a large (>3804 -> data stream) xattr on
    // dir00/f000.bin so the xattr read path can be proven on a real image.
    let xf = mnt.join("dir00").join("f000.bin");
    if let Some(p) = xf.to_str() {
        let _ = std::process::Command::new("xattr")
            .args(["-w", "user.apfsx_small", "hello-apfs", p])
            .output();
        let big = "Z".repeat(5000);
        let _ = std::process::Command::new("xattr")
            .args(["-w", "user.apfsx_big", &big, p])
            .output();
    }
    // M3d: embed GENUINE macOS LZVN (decmpfs method 8) samples + their
    // kernel-decompressed expected content, so the decoder is proven against
    // real macOS bytes vs macOS's own output. Two varied samples (a small
    // single-chunk + a larger multi-chunk) so the test is not over-fit to one
    // /usr/bin file. Best-effort (macOS only); skips parts not found.
    let mnt_str = mnt.to_str().unwrap_or("");
    // Build the Python script via concatenation so Rust's format! never sees
    // Python's {mnt!r} conversion specifier (which is not a Rust format arg).
    let py = "import os,sys,struct,ctypes,ctypes.util,subprocess\n".to_owned()
        + &format!("M={mnt_str:?}\n")
        + r#"libc=ctypes.CDLL(ctypes.util.find_library("c"),use_errno=True)
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
m8=[]
for p in cands:
    d=gx(p,"com.apple.decmpfs")
    if d and len(d)>=16 and struct.unpack_from('<I',d,4)[0]==8:
        u=struct.unpack_from('<Q',d,8)[0]
        if 0<u<=500000:
            r=gx(p,"com.apple.ResourceFork")
            if r: m8.append((u,p,d,r))
m8.sort()
small=next((x for x in m8 if x[0]<=65536), None)
large=next((x for x in m8 if x[0]>131072), None)
def emit(tag,sel):
    if not sel: print("M3D_FIXTURE_SKIP_%s"%tag); return
    u,p,d,r=sel
    content=open(p,'rb').read()
    if len(content)!=u: print("M3D_FIXTURE_SKIP_%s size_mismatch"%tag); return
    bn=os.path.join(M,"dir00","cmp_%s.bin"%tag); open(bn,'wb').write(b"\x00")
    sx(bn,"com.apple.decmpfs",d); sx(bn,"com.apple.ResourceFork",r)
    open(os.path.join(M,"dir00","cmp_%s.expected"%tag),'wb').write(content)
    print("M3D_FIXTURE_OK_%s src=%s usize=%d"%(tag,os.path.basename(p),u))
emit("s",small); emit("m",large)
"#;
    let _ = std::process::Command::new("python3")
        .arg("-c")
        .arg(&py)
        .status();
    let _ = std::process::Command::new("hdiutil")
        .args(["detach", mnt.to_str().ok_or("bad mnt")?])
        .output();
    let _ = std::fs::remove_dir_all(&mnt);
    verify_nxsb(&img)
}

/// Locate the APFS physical store device for a mounted volume, then print
/// the dd command to snapshot it and the env vars to run the differential test.
fn rt_snapshot(mount: &str) -> Result<String, String> {
    let info = std::process::Command::new("diskutil")
        .args(["info", mount])
        .output()
        .map_err(|e| format!("diskutil: {e}"))?;
    if !info.status.success() {
        return Err(format!("diskutil info {mount} failed"));
    }
    let text = String::from_utf8_lossy(&info.stdout);
    let mut store = String::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("APFS Physical Store:") {
            store = v.trim().to_string();
        } else if store.is_empty() {
            if let Some(v) = l.strip_prefix("Device Node:") {
                store = v.trim().trim_start_matches("/dev/").to_string();
            }
        }
    }
    if store.is_empty() {
        return Err(format!(
            "could not find an APFS Physical Store / Device Node for {mount} \
             (is it an APFS volume? run `diskutil info {mount}`)"
        ));
    }
    let raw = format!("/dev/r{store}");
    let out = "/tmp/apfs-rt.img";
    Ok(format!(
        "APFS container device for {mount}: /dev/{store}\n\
         One-time read-only snapshot (slow USB read once into a fast local image):\n\
         \n  sudo dd if={raw} of={out} bs=1m\n\n\
         Then run the differential (no sudo):\n\
         \n  APFS_RT_IMAGE={out} APFS_RT_MOUNT={mount} \\\n\
           cargo test --manifest-path \
         Cargo.toml \\\n\
           -p apfs-core --test real_target_differential -- --nocapture\n"
    ))
}

/// Confirm the APFS container superblock magic 'NXSB' is at file offset 32.
fn verify_nxsb(img: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(img).map_err(|e| e.to_string())?;
    let magic = bytes.get(32..36).ok_or("image too small")?;
    if magic != b"NXSB" {
        return Err(format!("offset 32 not NXSB: {magic:02x?}"));
    }
    Ok(img.display().to_string())
}

/// Create a disposable APFS-in-GPT scratch image at `path` (macOS only).
///
/// Empirically confirmed working invocation (macOS 14, hdiutil 5.0):
///
///   `hdiutil create -size <N>m -fs APFS -volname APFSWRITE -layout GPTSPUD <stem>`
///
/// `-layout GPTSPUD` produces a GPT partition map with a single Apple_APFS
/// partition, which is what fsck_apfs expects for a real APFS-in-GPT container.
/// hdiutil appends `.dmg` to the stem; we rename to the caller-supplied path.
///
/// `size_mb` is in mebibytes. Default is 64 MiB (well above the conservative
/// 32 MiB floor enforced by `apfs-write::scratch::ScratchSpec`).
fn gen_scratch(path: &str, size_mb: u64) -> Result<String, String> {
    let img_path = std::path::Path::new(path);

    // Remove any existing file at path.
    if img_path.exists() {
        std::fs::remove_file(img_path).map_err(|e| e.to_string())?;
    }

    // Build a stem without extension so hdiutil can append .dmg.
    // If the caller passed "foo.img" we strip the suffix; hdiutil will write "foo.dmg".
    let stem_path = img_path.with_extension("");
    let stem_str = stem_path.to_str().ok_or("non-UTF-8 path")?;

    let size_arg = format!("{size_mb}m");

    // Empirically confirmed: hdiutil create with -layout GPTSPUD produces a
    // real GPT + APFS container image accepted by hdiutil attach + fsck_apfs.
    let out = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            &size_arg,
            "-fs",
            "APFS",
            "-volname",
            "APFSWRITE",
            "-layout",
            "GPTSPUD",
            stem_str,
        ])
        .output()
        .map_err(|e| format!("hdiutil spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "hdiutil create: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    // hdiutil writes <stem>.dmg; rename to the caller-supplied path.
    let dmg_path = stem_path.with_extension("dmg");
    if dmg_path != img_path {
        std::fs::rename(&dmg_path, img_path).map_err(|e| {
            format!(
                "rename {} -> {}: {e}",
                dmg_path.display(),
                img_path.display()
            )
        })?;
    }

    Ok(format!(
        "scratch image created: {} ({size_mb} MiB, APFS-in-GPT)",
        img_path.display()
    ))
}

/// Attach a scratch image read-only, run `fsck_apfs -n`, list snapshots,
/// then detach - cleaning up even on error so no /dev/diskN is leaked.
///
/// Empirically confirmed attach invocation (macOS 14):
///
///   `hdiutil attach -readonly <path>`
///
/// Output format (4 lines for a GPT+APFS image):
///   /dev/diskN   GUID_partition_scheme
///   /dev/diskNs1 Apple_APFS
///   /dev/diskM   EF57347C-...   (synthesized APFS container)
///   /dev/diskMs1 41504653-...   /Volumes/APFSWRITE
///
/// fsck_apfs is run on the synthesized container device (/dev/diskM).
/// diskutil apfs listSnapshots is run against the volume mountpoint.
/// hdiutil detach is called on the parent GPT device (/dev/diskN).
fn verify_scratch(path: &str) -> Result<String, String> {
    // --- Attach read-only ---
    let att = Command::new("hdiutil")
        .args(["attach", "-readonly", path])
        .output()
        .map_err(|e| format!("hdiutil attach spawn: {e}"))?;
    if !att.status.success() {
        return Err(format!(
            "hdiutil attach: {}",
            String::from_utf8_lossy(&att.stderr)
        ));
    }

    let att_text = String::from_utf8_lossy(&att.stdout);

    // Parse attach output: extract parent device, APFS container device, mountpoint.
    // Each line: "<device>\t<type>\t[mountpoint]"
    let mut parent_dev: Option<String> = None; // GUID_partition_scheme line
    let mut apfs_container: Option<String> = None; // EF57347C line (synthesized container)
    let mut mountpoint: Option<String> = None; // 41504653 line mountpoint

    for line in att_text.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').map(str::trim).collect();
        let (Some(dev), Some(kind)) = (parts.first(), parts.get(1)) else {
            continue;
        };
        if kind.contains("GUID_partition_scheme") && parent_dev.is_none() {
            parent_dev = Some((*dev).to_string());
        } else if kind.starts_with("EF57347C") && apfs_container.is_none() {
            apfs_container = Some((*dev).to_string());
        } else if kind.starts_with("41504653") {
            if let Some(mnt) = parts.get(2) {
                if !mnt.is_empty() {
                    mountpoint = Some((*mnt).to_string());
                }
            }
        }
    }

    let parent_dev = parent_dev.ok_or_else(|| {
        format!("could not find GUID_partition_scheme device in hdiutil attach output:\n{att_text}")
    })?;
    let apfs_container = apfs_container.ok_or_else(|| {
        // Detach best-effort before returning error.
        let _ = Command::new("hdiutil")
            .args(["detach", &parent_dev])
            .output();
        format!("could not find APFS container device (EF57347C) in attach output:\n{att_text}")
    })?;
    let mountpoint = mountpoint.ok_or_else(|| {
        let _ = Command::new("hdiutil")
            .args(["detach", &parent_dev])
            .output();
        format!("could not find volume mountpoint in attach output:\n{att_text}")
    })?;

    // --- fsck_apfs -n on the container ---
    // Empirically confirmed: fsck_apfs -n /dev/diskM (synthesized container device)
    let fsck = Command::new("fsck_apfs")
        .args(["-n", &apfs_container])
        .output();

    let fsck_result = match fsck {
        Ok(o) => o,
        Err(e) => {
            let _ = Command::new("hdiutil")
                .args(["detach", &parent_dev])
                .output();
            return Err(format!("fsck_apfs spawn: {e}"));
        }
    };

    let fsck_out = String::from_utf8_lossy(&fsck_result.stdout);
    // fsck_apfs -n returns exit 0 and prints "appears to be OK" even when the
    // container has space-accounting problems - those surface only as "warning:"
    // / "Fix …?" lines (e.g. "overallocation detected", "apfs_fs_alloc_count is
    // not valid"). Treating those as CLEAN masked a real spaceman allocator bug
    // for the whole M7 write series. Container-level allocation integrity is a
    // hard requirement, so any of these markers makes the result DIRTY.
    // [CERTAIN: empirical - fsck warnings on a structurally-OK volume still mean
    //  the on-disk allocation state is wrong; real disks must not accumulate this.]
    let has_alloc_fault = fsck_out.contains("overallocation")
        || fsck_out.contains("is not valid")
        || fsck_out.contains("was found to be corrupt")
        || fsck_out.contains("is invalid");
    let fsck_clean = fsck_result.status.success()
        && fsck_out.contains("appears to be OK")
        && !fsck_out.contains("error")
        && !has_alloc_fault;
    let status_str = if fsck_clean { "CLEAN" } else { "DIRTY" };

    // --- diskutil apfs listSnapshots on the mountpoint ---
    // Empirically confirmed: diskutil apfs listSnapshots <mountpoint>
    let snaps = Command::new("diskutil")
        .args(["apfs", "listSnapshots", &mountpoint])
        .output();

    let snaps_text = match snaps {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => format!("(diskutil listSnapshots error: {e})"),
    };

    // --- Detach (always, even on fsck dirty) ---
    let det = Command::new("hdiutil")
        .args(["detach", &parent_dev])
        .output()
        .map_err(|e| format!("hdiutil detach spawn: {e}"))?;
    if !det.status.success() {
        return Err(format!(
            "hdiutil detach {parent_dev}: {}",
            String::from_utf8_lossy(&det.stderr)
        ));
    }

    if !fsck_clean {
        return Err(format!(
            "fsck_apfs reported DIRTY on {apfs_container}:\n{fsck_out}"
        ));
    }

    Ok(format!(
        "status: {status_str}\ncontainer: {apfs_container}\nsnapshots:\n{snaps_text}"
    ))
}

/// Check whether WinFsp is installed on this Windows box.
///
/// Reads `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir`. Returns `Ok(path)`
/// when present, `Err` when absent or unreadable.
///
/// This check mirrors the registry variable search used in `wix/bundle.wxs`
/// so the CLI and the Burn bootstrapper use the same signal.
#[cfg(windows)]
fn winfsp_installed() -> Result<String, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows::core::w;
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
        REG_SZ, REG_VALUE_TYPE,
    };
    unsafe {
        let mut hkey = HKEY::default();
        // windows 0.58: RegOpenKeyExW returns WIN32_ERROR (not Result); compare
        // directly against NO_ERROR instead of using .map_err().
        let rc = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\WOW6432Node\\WinFsp"),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        );
        if rc != NO_ERROR {
            return Err(format!("WinFsp registry key not found: {rc:?}"));
        }

        let mut buf = vec![0u16; 512];
        let mut cb = (buf.len() * 2) as u32;
        // REG_VALUE_TYPE is a newtype wrapper; pass via raw pointer cast.
        let mut kind = REG_VALUE_TYPE(0);
        let result = RegQueryValueExW(
            hkey,
            w!("InstallDir"),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut cb),
        );
        let _ = RegCloseKey(hkey);
        if result != NO_ERROR {
            return Err(format!("WinFsp InstallDir not found: {result:?}"));
        }
        if kind != REG_SZ {
            return Err("WinFsp InstallDir is not REG_SZ".into());
        }
        // Strip trailing NULs then decode.
        let word_len = (cb / 2) as usize;
        let s = std::ffi::OsString::from_wide(&buf[..word_len])
            .to_string_lossy()
            .trim_end_matches('\0')
            .to_string();
        Ok(s)
    }
}

/// Build the one-installer Burn bundle around the Tauri product MSI.
///
/// Pivot B layout (single-EXE GUI): the product MSI is built by `cargo tauri
/// build` (GUI `apfs-explorer.exe` + bundled `apfs-mount.exe` resource + the
/// `wix/winfsp-launcher.wxs` fragment that registers the WinFsp launcher
/// class). This function wraps that MSI in a WiX Burn bundle
/// (`dist/apfs-explorer-setup.exe`) that also chains the official WinFsp MSI,
/// installing WinFsp silently when absent. (The old service-mode/apfs-tray
/// `wix/main.wxs` product MSI was retired with the tray crate in Pivot B.)
///
/// Windows-only: WiX v3 + the tauri CLI live on the box. On other OSes, print
/// the exact recipe and succeed (keeps `cargo run -p xtask` usable from macOS).
///
/// # WinFsp prerequisite
///
/// The Burn bundle chains the official WinFsp installer (`dist/winfsp.msi`) -
/// it installs WinFsp silently when absent. Stage the WinFsp MSI first:
///   1. Download from <https://winfsp.dev/rel/> (latest stable, e.g. 2.0).
///   2. Copy the MSI to `dist/winfsp.msi` in the repo root.
///   3. Run `cargo xtask package`.
///
/// The MSI is NOT redistributed in the repo (signed-driver redistribution
/// requires WinFsp's own license; we chain it, not embed it).
fn package() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(not(windows))]
    {
        println!(
            "xtask package is Windows-only. On the box run:\n\
             \n\
             Prerequisites:\n  \
             1. Stage official WinFsp MSI: copy to dist/winfsp.msi\n  \
                Download: https://winfsp.dev/rel/\n\
             \n\
             Build steps (from the repo root):\n  \
             cargo build --release -p winfsp-host --bin apfs-mount\n  \
             copy target/release/apfs-mount.exe crates/apfs-explorer-gui/apfs-mount.exe\n  \
             (cd crates/apfs-explorer-gui && cargo tauri build)   # -> Tauri product MSI\n  \
             copy \"<tauri msi>\" dist/apfs-explorer.msi\n  \
             candle -arch x64 -ext WixBalExtension -ext WixUtilExtension \\\n    \
               -dVersion=<ver> wix/bundle.wxs -o dist/bundle.wixobj\n  \
             light -ext WixBalExtension -ext WixUtilExtension \\\n    \
               dist/bundle.wixobj -o dist/apfs-explorer-setup.exe\n\
             \n\
             Output: dist/apfs-explorer-setup.exe (Burn bundle, chains WinFsp +\n  \
             installs the Tauri product MSI)"
        );
        Ok(())
    }
    #[cfg(windows)]
    {
        // Prerequisite: dist/winfsp.msi must be staged by the operator.
        if !std::path::Path::new("dist/winfsp.msi").exists() {
            return Err("dist/winfsp.msi not found.\n\
                 Download the official WinFsp MSI from https://winfsp.dev/rel/ \
                 and copy it to dist/winfsp.msi, then re-run xtask package."
                .into());
        }

        // Informational: report whether WinFsp is already installed on this box.
        match winfsp_installed() {
            Ok(dir) => println!("WinFsp detected at: {dir}"),
            Err(e) => println!(
                "WinFsp not detected on this box ({e}); \
                 the bundle will install it on the target machine."
            ),
        }

        std::fs::create_dir_all("dist").map_err(|e| e.to_string())?;

        // Step 1: build apfs-mount and stage it as the Tauri resource (the
        // GUI bundle embeds crates/apfs-explorer-gui/apfs-mount.exe - see
        // tauri.conf.json `bundle.resources`).
        let status = Command::new("cargo")
            .args([
                "build",
                "--release",
                "-p",
                "winfsp-host",
                "--bin",
                "apfs-mount",
            ])
            .status()?;
        if !status.success() {
            return Err("cargo build apfs-mount failed".into());
        }
        std::fs::copy(
            "target/release/apfs-mount.exe",
            "crates/apfs-explorer-gui/apfs-mount.exe",
        )
        .map_err(|e| format!("stage apfs-mount.exe as Tauri resource: {e}"))?;

        // Step 2: build the product MSI via the Tauri bundler. Remove any stale
        // MSI from a prior build FIRST so Step 3 cannot pick up an old version
        // (the bundle/msi dir is not cleaned by `cargo tauri build`).
        let msi_dir = "target/release/bundle/msi";
        let _ = std::fs::remove_dir_all(msi_dir);
        let status = Command::new("cargo")
            .args(["tauri", "build"])
            .current_dir("crates/apfs-explorer-gui")
            .status()?;
        if !status.success() {
            return Err("cargo tauri build failed (is the tauri CLI installed?)".into());
        }

        // Step 3: copy the produced MSI to the stable path the bundle chains
        // (dist/apfs-explorer.msi). The Tauri output name encodes productName +
        // version (e.g. "APFS Explorer_1.0.2_x64_en-US.msi"); pick the most
        // recently modified .msi so a non-empty dir can never select a stale one.
        let produced = std::fs::read_dir(msi_dir)
            .map_err(|e| format!("read {msi_dir}: {e} (did `cargo tauri build` produce an MSI?)"))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("msi"))
            })
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .map(|e| e.path())
            .ok_or_else(|| format!("no .msi in {msi_dir}"))?;
        std::fs::copy(&produced, "dist/apfs-explorer.msi")
            .map_err(|e| format!("copy product MSI to dist/apfs-explorer.msi: {e}"))?;
        println!(
            "product MSI: {} -> dist/apfs-explorer.msi",
            produced.display()
        );

        // Step 4: Burn bundle (chains WinFsp + the product MSI) via candle/light.
        // Bundle version tracks the Tauri product version in tauri.conf.json so
        // the bundle and the MSI it carries report the same version.
        let version = tauri_conf_version()
            .unwrap_or_else(|_| installer::installer_version(env!("CARGO_PKG_VERSION")));
        let dver = format!("-dVersion={version}");
        let status = Command::new("candle")
            .args([
                "-arch",
                "x64",
                "-ext",
                "WixBalExtension",
                "-ext",
                "WixUtilExtension",
                &dver,
                "wix/bundle.wxs",
                "-o",
                "dist/bundle.wixobj",
            ])
            .status()?;
        if !status.success() {
            return Err("candle bundle.wxs failed".into());
        }
        let status = Command::new("light")
            .args([
                "-ext",
                "WixBalExtension",
                "-ext",
                "WixUtilExtension",
                "dist/bundle.wixobj",
                "-o",
                "dist/apfs-explorer-setup.exe",
            ])
            .status()?;
        if !status.success() {
            return Err("light bundle.wxs failed".into());
        }

        println!("OK dist/apfs-explorer-setup.exe");
        println!(
            "  - chains dist/winfsp.msi (installs WinFsp when absent)\n  \
             - installs the Tauri product MSI (apfs-explorer.exe + apfs-mount.exe\n    \
               + WinFsp launcher class registration)"
        );
        Ok(())
    }
}

/// Read the `"version"` string from `crates/apfs-explorer-gui/tauri.conf.json`
/// and normalise it to the MSI 4-part form via [`installer::installer_version`].
/// Minimal hand parse (no JSON dep): finds the first `"version"` key.
#[cfg(windows)]
fn tauri_conf_version() -> Result<String, Box<dyn std::error::Error>> {
    let conf = std::fs::read_to_string("crates/apfs-explorer-gui/tauri.conf.json")?;
    let key = "\"version\"";
    let after = conf
        .find(key)
        .and_then(|i| {
            conf[i + key.len()..]
                .find('"')
                .map(|j| i + key.len() + j + 1)
        })
        .ok_or("no \"version\" key in tauri.conf.json")?;
    let end = conf[after..]
        .find('"')
        .ok_or("unterminated version string")?;
    Ok(installer::installer_version(&conf[after..after + end]))
}
