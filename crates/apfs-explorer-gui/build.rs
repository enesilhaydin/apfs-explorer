// Tauri 2 build glue: reads tauri.conf.json + capabilities and emits
// the generated context that `tauri::generate_context!()` consumes in main.rs.
// The Windows manifest (app.manifest) makes the app ask for elevation once at
// start, and APFS_BUILD_ID names the commit and date the app was built from.
fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rustc-env=APFS_BUILD_ID={}", build_id());
    let windows = tauri_build::WindowsAttributes::new().app_manifest(include_str!("app.manifest"));
    if let Err(e) =
        tauri_build::try_build(tauri_build::Attributes::new().windows_attributes(windows))
    {
        panic!("tauri build: {e:#}");
    }
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
