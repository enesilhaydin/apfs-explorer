//! winfsp-host build script.
//!
//! winfsp-rs mandates delay-loading of `winfsp-x64.dll`. This emits the
//! linker flags so the WinFsp imports are delay-loaded; at runtime
//! `winfsp::winfsp_init()` calls `FspLoad`, which locates and loads the
//! DLL from the WinFsp install path recorded in the registry. Without
//! this, the binary fails to start with STATUS_DLL_NOT_FOUND (0xC0000135)
//! before `main` runs (the import would be a hard, non-delay dependency).
//!
//! Only relevant when building natively on Windows (host == target);
//! on macOS the `winfsp` build-dependency is absent (target-gated in
//! Cargo.toml) and this is a no-op.
//!
//! It also sets `APFS_BUILD_ID` (see `build_id`), which `apfs-mount
//! --version` and the log's start line print.
fn main() {
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
    println!("cargo:rustc-env=APFS_BUILD_ID={}", build_id());
    println!("cargo:rustc-env=APFS_PRODUCT_VERSION={}", product_version());
}

/// The product's version: the one `xtask set-version` writes to the GUI's
/// `tauri.conf.json` and the installer carries. The crates themselves stay
/// at 0.0.0.
fn product_version() -> String {
    let conf = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../apfs-explorer-gui/tauri.conf.json"
    );
    println!("cargo:rerun-if-changed={conf}");
    std::fs::read_to_string(conf)
        .ok()
        .and_then(|c| {
            let after = c.split_once("\"version\"")?.1;
            let value = after.split_once('"')?.1;
            Some(value.split_once('"')?.0.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// `<commit>[-modified], built <YYYY-MM-DD>`: the commit the binary was
/// built from (with `-modified` when tracked files differed from it) and
/// the UTC build date. `APFS_BUILD_COMMIT` names the commit where there is
/// no `.git` (a source export); without either it is `unknown`.
fn build_id() -> String {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    println!("cargo:rerun-if-env-changed=APFS_BUILD_COMMIT");
    let git_dir = format!("{root}/.git");
    for f in ["HEAD", "index", "packed-refs"] {
        println!("cargo:rerun-if-changed={git_dir}/{f}");
    }
    if let Ok(head) = std::fs::read_to_string(format!("{git_dir}/HEAD")) {
        if let Some(r) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed={git_dir}/{r}");
        }
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
    };
    let commit = match std::env::var("APFS_BUILD_COMMIT") {
        Ok(c) if !c.trim().is_empty() => c.trim().to_string(),
        _ => match git(&["rev-parse", "--short=10", "HEAD"]) {
            Some(o) if o.status.success() => {
                let c = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let clean =
                    git(&["diff", "--quiet", "HEAD", "--"]).is_some_and(|o| o.status.success());
                if clean {
                    c
                } else {
                    format!("{c}-modified")
                }
            }
            _ => "unknown".to_string(),
        },
    };
    format!("{commit}, built {}", utc_date())
}

/// Today's UTC date as `YYYY-MM-DD`, without a date crate.
fn utc_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Civil-from-days (Howard Hinnant's algorithm), days since 1970-01-01.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
