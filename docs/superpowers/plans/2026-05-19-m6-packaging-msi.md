# M6 — Packaging (one-installer MSI/Bundle) + Robustness Scenario Pass

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A single signed installer that puts WinFsp + `apfs-tray.exe` + `apfs-mount.exe` on an end-user Windows machine with per-user autostart, so "install → plug an APFS USB → it shows up as a drive" works with zero manual setup; plus an explicit robustness/failure-scenario test pass over the realistic v1 (read-only) failure modes on real hardware.

**Architecture:** `cargo-wix` generates a per-product MSI (`apfs-explorer.msi`) containing both binaries under Program Files + an HKCU `Run` registry value for autostart. A WiX **Burn bundle** (`apfs-explorer-setup.exe`) chains the upstream **WinFsp redistributable MSI** (skipped if already present, via a registry search) then our MSI — one double-click for the user. Signing via `signtool` (test cert acceptable for CI/dev; production cert is an operator input). A new `xtask` subcommand drives the build deterministically. The robustness pass is a host-side scripted scenario matrix run against the real APFS USB on the SSH box.

**Tech Stack:** Rust 2021, `cargo-wix` (WiX v3 Toolset: candle/light), WiX Burn (`<Bundle>`), `signtool`, existing `apfs-tray`/`winfsp-host` binaries, the SSH Windows box (`apfswin`).

**Scope decision:** packaging is build/release tooling, not library code — most of it is WiX XML + an xtask driver + box verification, not unit-testable Rust logic. The one piece of pure logic (installer **version string derivation** from Cargo metadata) is unit-tested on macOS. Everything else is proven by actually building the MSI/bundle on the box and installing/uninstalling it. The robustness pass is woven in here (task #50 realistic subset) because M6 is the first point the product is shipped as a unit and "does it survive abuse" must be answered before release.

**Out of scope (tracked elsewhere):** write-path crash safety = M7; exhaustive fuzzing / power-loss simulation / long soak = M8 (#50 lists the full matrix; M6 covers only the read-only scenarios testable now). #49 / #44 / #45 remain separate.

**Source-of-truth discipline:** exact WiX element attributes, `cargo-wix` flags, WinFsp MSI ProductCode/UpgradeCode and its registry marker are HYPOTHESIS until resolved on the box against the installed WiX + the real WinFsp 2.1 MSI; the box build + a real install/uninstall is the arbiter. Record resolved values in comments in the WiX sources. No APFS on-disk spec is touched (no md.6).

**Box facts (from M4b/M5, durable):** SSH alias `apfswin` (key auth, no password). WinFsp 2.1 installed at `C:\Program Files (x86)\WinFsp\`; its registry marker `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir`. Rust 1.91 MSVC + git. Repo at `C:\apfs\apfs-explorer`. Real APFS USB on `\\.\PhysicalDrive3` (root contains `man` + macOS metadata). `apfs-tray.exe` auto-detects + spawns out-of-process `apfs-mount.exe` (Job Object KILL_ON_JOB_CLOSE); auto free drive-letter.

---

## File Structure

- `crates/installer/Cargo.toml` + `src/lib.rs` — tiny crate: pure `installer_version()` (Cargo version → 4-part WiX version) + macOS unit test. (No Windows deps; builds everywhere.)
- `wix/main.wxs` — WiX v3 `<Product>`: both exes (one File/Component) + HKCU Run autostart component + feature.
- `wix/bundle.wxs` — WiX Burn `<Bundle>`: RegistrySearch for WinFsp → chain WinFsp MSI (conditional) → our MSI.
- `xtask/src/main.rs` — add `package` subcommand: build release binaries → `cargo wix` → build the Burn bundle → optional `signtool` → emit `dist/apfs-explorer-setup.exe`.
- `scripts/robustness.ps1` — host scenario-matrix script (committed; run on the box in the final task).
- `docs/ROADMAP.md` — M6 status (final task).

The only macOS-unit-tested unit is `installer::installer_version`. WiX/bundle/signing/robustness are proven on the box (final task) — consistent with the project's "real environment is the arbiter" methodology.

---

### Task 1: `installer` crate — pure version derivation (macOS-tested) + workspace wiring

**Files:**
- Create: `crates/installer/Cargo.toml`, `crates/installer/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

- [ ] **Step 1: Write the failing test**

`crates/installer/src/lib.rs`:

```rust
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Installer helpers. Pure version-string derivation (unit-tested);
//! the actual MSI/bundle build lives in `xtask package` + `wix/`.

/// Convert a Cargo SemVer string ("0.0.0", "1.2.3", "1.2.3-rc.1") into a
/// WiX/MSI 4-part numeric version "MAJOR.MINOR.PATCH.0". MSI ignores
/// pre-release tags and requires numeric fields, so they are stripped.
pub fn installer_version(cargo_version: &str) -> String {
    let core = cargo_version
        .split(['-', '+'])
        .next()
        .unwrap_or(cargo_version);
    let mut parts: Vec<&str> = core.split('.').collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    let n = |s: &str| s.chars().take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    let major = n(parts.first().copied().unwrap_or("0"));
    let minor = n(parts.get(1).copied().unwrap_or("0"));
    let patch = n(parts.get(2).copied().unwrap_or("0"));
    let d = |s: String| if s.is_empty() { "0".to_string() } else { s };
    format!("{}.{}.{}.0", d(major), d(minor), d(patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_semver() {
        assert_eq!(installer_version("1.2.3"), "1.2.3.0");
    }

    #[test]
    fn zero_version() {
        assert_eq!(installer_version("0.0.0"), "0.0.0.0");
    }

    #[test]
    fn strips_prerelease_and_build() {
        assert_eq!(installer_version("1.4.0-rc.1"), "1.4.0.0");
        assert_eq!(installer_version("2.0.0+win"), "2.0.0.0");
    }

    #[test]
    fn pads_short_version() {
        assert_eq!(installer_version("1"), "1.0.0.0");
        assert_eq!(installer_version("1.5"), "1.5.0.0");
    }

    #[test]
    fn non_numeric_field_falls_back_to_zero() {
        assert_eq!(installer_version("x.y.z"), "0.0.0.0");
    }
}
```

`crates/installer/Cargo.toml`:

```toml
[package]
name = "installer"
version = "0.0.0"
edition.workspace = true
license.workspace = true

[lints]
workspace = true
```

Add `"crates/installer",` to the workspace `members` list in the root `Cargo.toml` (explicit edit; keep formatting).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p installer`
Expected: FAIL first if you stage only the tests; with the impl present it compiles — to honor red→green, temporarily replace the body with `String::new()`, observe `plain_semver` FAIL, then restore.

- [ ] **Step 3: Write minimal implementation** — the body above is the implementation.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p installer` — Expected: PASS (5 tests).
Run: `cargo build --workspace` + `cargo clippy -p installer --all-targets -- -D warnings` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/installer/Cargo.toml crates/installer/src/lib.rs Cargo.toml
git commit -m "feat(installer): pure Cargo-to-WiX version derivation with tests"
```

---

### Task 2: `wix/main.wxs` — product MSI (both exes + HKCU autostart)

**Files:**
- Create: `wix/main.wxs`

- [ ] **Step 1: Write the WiX product**

Create `wix/main.wxs` (GUIDs are placeholders to be replaced with freshly generated GUIDs in Step 2; `cargo wix init` can also scaffold — we author explicitly for the two-binary + autostart layout):

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!-- apfs-explorer product MSI. Built by `cargo wix` (WiX v3).
     Resolve/confirm on the box: cargo-wix expects this at wix/main.wxs;
     binary source paths are the release build outputs. Replace every
     PUT-GUID with a generated GUID (uuidgen); UpgradeCode is STABLE
     across versions, component GUIDs are stable per component. -->
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
  <Product Id="*"
           Name="apfs-explorer"
           Manufacturer="apfs-explorer"
           Version="$(var.Version)"
           UpgradeCode="PUT-STABLE-UPGRADE-GUID"
           Language="1033">
    <Package InstallerVersion="500" Compressed="yes"
             InstallScope="perMachine" />
    <MajorUpgrade DowngradeErrorMessage=
      "A newer version of apfs-explorer is already installed." />
    <MediaTemplate EmbedCab="yes" />

    <Directory Id="TARGETDIR" Name="SourceDir">
      <Directory Id="ProgramFiles64Folder">
        <Directory Id="INSTALLFOLDER" Name="apfs-explorer">
          <Component Id="C_ApfsTray" Guid="PUT-TRAY-GUID">
            <File Id="F_ApfsTray"
                  Source="$(var.TrayExe)"
                  KeyPath="yes" Checksum="yes" />
          </Component>
          <Component Id="C_ApfsMount" Guid="PUT-MOUNT-GUID">
            <File Id="F_ApfsMount"
                  Source="$(var.MountExe)"
                  KeyPath="yes" Checksum="yes" />
          </Component>
          <!-- Per-user autostart: HKCU Run -> apfs-tray. Per-user so no
               UAC at logon; the agent runs in the user session (M5). -->
          <Component Id="C_Autostart" Guid="PUT-AUTOSTART-GUID">
            <RegistryValue
              Root="HKCU"
              Key="Software\Microsoft\Windows\CurrentVersion\Run"
              Name="apfs-tray"
              Type="string"
              Value="&quot;[INSTALLFOLDER]apfs-tray.exe&quot;"
              KeyPath="yes" />
          </Component>
        </Directory>
      </Directory>
    </Directory>

    <Feature Id="Main" Title="apfs-explorer" Level="1">
      <ComponentRef Id="C_ApfsTray" />
      <ComponentRef Id="C_ApfsMount" />
      <ComponentRef Id="C_Autostart" />
    </Feature>
  </Product>
</Wix>
```

- [ ] **Step 2: Generate real GUIDs**

Replace `PUT-STABLE-UPGRADE-GUID`, `PUT-TRAY-GUID`, `PUT-MOUNT-GUID`, `PUT-AUTOSTART-GUID` with four distinct generated GUIDs (`uuidgen` on macOS: `uuidgen`). The UpgradeCode must never change again (record a comment: "STABLE — do not regenerate").

- [ ] **Step 3: Sanity-check XML well-formedness**

Run (macOS): `python3 -c "import xml.dom.minidom,sys; xml.dom.minidom.parse('wix/main.wxs')"` — Expected: no error (well-formed). Full WiX compile happens on the box (Task 5).

- [ ] **Step 4: Commit**

```bash
git add wix/main.wxs
git commit -m "feat(wix): product MSI authoring (apfs-tray + apfs-mount + HKCU autostart)"
```

---

### Task 3: `wix/bundle.wxs` — Burn bundle chaining WinFsp + our MSI

**Files:**
- Create: `wix/bundle.wxs`

- [ ] **Step 1: Write the Burn bundle**

Create `wix/bundle.wxs`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!-- One-installer bundle: ensure WinFsp is present (install the bundled
     redistributable MSI only if its registry marker is absent), then
     install our product MSI. Resolve on the box: the exact WinFsp MSI
     filename/path (operator stages the official WinFsp 2.x MSI at
     dist/winfsp.msi) and confirm the registry marker key/value. -->
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi"
     xmlns:util="http://schemas.microsoft.com/wix/UtilExtension"
     xmlns:bal="http://schemas.microsoft.com/wix/BalExtension">
  <Bundle Name="apfs-explorer"
          Version="$(var.Version)"
          Manufacturer="apfs-explorer"
          UpgradeCode="PUT-STABLE-BUNDLE-UPGRADE-GUID">
    <BootstrapperApplicationRef
      Id="WixStandardBootstrapperApplication.RtfLicense">
      <bal:WixStandardBootstrapperApplication
        LicenseFile="wix\license.rtf" />
    </BootstrapperApplicationRef>

    <!-- WinFsp present? marker = its WOW6432Node InstallDir value. -->
    <util:RegistrySearch
      Id="WinFspInstalled"
      Variable="WinFspInstalled"
      Root="HKLM"
      Key="SOFTWARE\WOW6432Node\WinFsp"
      Value="InstallDir"
      Result="exists" />

    <Chain>
      <MsiPackage Id="WinFsp"
                  SourceFile="dist\winfsp.msi"
                  Permanent="yes"
                  Vital="yes"
                  InstallCondition="NOT WinFspInstalled" />
      <MsiPackage Id="ApfsExplorer"
                  SourceFile="dist\apfs-explorer.msi"
                  Vital="yes" />
    </Chain>
  </Bundle>
</Wix>
```

Add a minimal `wix/license.rtf` (one line: "apfs-explorer — GPL-3.0-only. See repository LICENSE." saved as RTF).

- [ ] **Step 2: Generate the bundle UpgradeCode GUID** (distinct from the product's). Replace `PUT-STABLE-BUNDLE-UPGRADE-GUID`; comment "STABLE".

- [ ] **Step 3: XML well-formed check** (macOS): same `xml.dom.minidom.parse('wix/bundle.wxs')` — Expected: no error.

- [ ] **Step 4: Commit**

```bash
git add wix/bundle.wxs wix/license.rtf
git commit -m "feat(wix): Burn bundle chaining WinFsp redistributable + product MSI"
```

---

### Task 4: `xtask package` — deterministic build driver

**Files:**
- Modify: `xtask/src/main.rs`

- [ ] **Step 1: Add the `package` subcommand**

Add to `xtask/src/main.rs` a `package` subcommand that (Windows-only execution; on macOS it prints the recipe and exits 0 so the workspace build/test is unaffected):

```rust
// In the subcommand dispatch match, add:
//   "package" => package()?,
// (keep existing subcommands intact)

fn package() -> Result<(), Box<dyn std::error::Error>> {
    // Windows-only: WiX v3 + signtool live on the box. On other OSes,
    // print the exact recipe and succeed (keeps `cargo run -p xtask`
    // usable from macOS for docs/CI of the non-Windows path).
    #[cfg(not(windows))]
    {
        println!(
            "xtask package is Windows-only. On the box run:\n  \
             cargo build --release -p winfsp-host --bin apfs-mount\n  \
             cargo build --release -p apfs-tray --bin apfs-tray\n  \
             cargo wix -p apfs-tray --nocapture (uses wix/main.wxs)\n  \
             candle/light wix/bundle.wxs -> dist/apfs-explorer-setup.exe\n  \
             signtool sign /fd SHA256 dist/apfs-explorer-setup.exe"
        );
        return Ok(());
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        let run = |c: &str, a: &[&str]| -> Result<(), Box<dyn std::error::Error>> {
            let st = Command::new(c).args(a).status()?;
            if !st.success() {
                return Err(format!("{c} {a:?} failed: {st}").into());
            }
            Ok(())
        };
        run("cargo", &["build", "--release", "-p", "winfsp-host",
            "--bin", "apfs-mount"])?;
        run("cargo", &["build", "--release", "-p", "apfs-tray",
            "--bin", "apfs-tray"])?;
        // cargo-wix consumes wix/main.wxs; -C/-D pass File source vars.
        run("cargo", &["wix", "-p", "apfs-tray", "--no-build",
            "--nocapture", "--output", "dist/apfs-explorer.msi",
            "-C", "-dTrayExe=target/release/apfs-tray.exe",
            "-C", "-dMountExe=target/release/apfs-mount.exe"])?;
        // Burn bundle (requires WiX candle/light + winfsp.msi staged).
        run("candle", &["-ext", "WixBalExtension", "-ext",
            "WixUtilExtension", "-dVersion=0.0.0.0",
            "wix/bundle.wxs", "-o", "dist/bundle.wixobj"])?;
        run("light", &["-ext", "WixBalExtension", "-ext",
            "WixUtilExtension", "dist/bundle.wixobj", "-o",
            "dist/apfs-explorer-setup.exe"])?;
        println!("OK dist/apfs-explorer-setup.exe");
        Ok(())
    }
}
```

(Exact `cargo wix` flags / candle-light invocation are resolved on the box in Task 5 — the compiler/toolset is the arbiter; keep the step sequence: build release bins → product MSI → bundle. Use `installer::installer_version` to pass the real `-dVersion=` once the crate version is bumped from 0.0.0; for now 0.0.0.0 is fine.)

- [ ] **Step 2: Verify macOS path**

Run: `cargo run -p xtask -- package` (macOS) — Expected: prints the recipe, exits 0.
Run: `cargo build --workspace` — Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add xtask/src/main.rs
git commit -m "feat(xtask): package subcommand driving MSI + Burn bundle build"
```

---

### Task 5: Box — install WiX, build the one-installer, install/uninstall proof

**Files:** none (operator/box actions; findings recorded in Task 7 ROADMAP notes)

- [ ] **Step 1: Provision WiX on the box** (SSH `apfswin`): `winget install --id WiXToolset.WiXToolset -e --silent --accept-package-agreements --accept-source-agreements` (or install WiX v3.11 + add to PATH); `cargo install cargo-wix` (idempotent). Stage the official WinFsp 2.x MSI at `C:\apfs\apfs-explorer\dist\winfsp.msi` (operator downloads it once; record the version).

- [ ] **Step 2: Build the installer on the box** (sync repo via the M4b/M5 bundle recipe, then): `cargo run -p xtask -- package`. Iterate ONLY on WiX/cargo-wix invocation + WiX XML attribute mismatches (compiler/toolset arbiter); record resolved flags/attrs/WinFsp marker in `wix/*.wxs` comments + commit on the Mac. End state: `dist/apfs-explorer-setup.exe` produced.

- [ ] **Step 3: Clean-machine-ish install proof.** On the box: confirm pre-state (note whether WinFsp already installed — it is). Run `dist/apfs-explorer-setup.exe /quiet`. Assert: both exes present under `%ProgramFiles%\apfs-explorer\`, HKCU Run `apfs-tray` value set, WinFsp present (bundle skipped it via registry search since already installed — verify the skip logic by checking the bundle log). Launch `apfs-tray.exe` (as the autostart would at logon) and confirm the real APFS USB auto-mounts to a free letter (root lists `man`). Then `msiexec /x` (or the bundle uninstall) and assert: exes removed, HKCU Run value removed, agent/child stopped, drive gone, WinFsp left intact (Permanent="yes").

- [ ] **Step 4: Record outcomes** for Task 7 (resolved WiX flags, WinFsp MSI version, install/uninstall results).

(No commit — verification task. Any source fixes are committed under the relevant Task 2/3/4 file with a clear message.)

---

### Task 6: Robustness scenario pass (realistic v1 read-only failure modes) on the box

**Files:**
- Create: `scripts/robustness.ps1`

This is the explicit "does it survive abuse" pass (task #50 subset testable now; write-crash = M7, fuzz/power-loss = M8). Each scenario asserts the product invariants: **no panic / no BSOD / no hang; clean error or graceful unmount; no orphaned `apfs-mount`; agent stays alive (fault isolation)**.

- [ ] **Step 1: Write the scenario script**

Create `scripts/robustness.ps1` (committed; run on the box). It drives, against the real USB on `\\.\PhysicalDrive3`, with the installed product:

```
# Scenario matrix (read-only, v1). Each prints PASS/FAIL <name>.
# S1 surprise-removal-mid-read: start a large recursive copy/hash from the
#    mounted drive; midway, force-offline the disk
#    (diskpart "offline disk" / Remove-PnpDevice on the USB) -> assert:
#    apfs-tray still alive, apfs-mount child exits, drive disappears,
#    no Windows error dialog / no bugcheck; copy aborts with an I/O error
#    (NOT a hang, NOT a crash of the agent).
# S2 agent-force-kill-mid-mount: kill apfs-tray with /F while a read is in
#    flight -> assert: Job Object kills apfs-mount too, drive gone, no
#    orphan process, box clean.
# S3 rapid-replug x5: offline/online the disk 5x quickly -> assert: every
#    cycle re-mounts to a (possibly new) free letter, no leaked child,
#    no leaked drive letter, agent never dies.
# S4 slow-device: throttle reads (large file copy) -> assert: mount stays
#    responsive enough, no hang/timeout-crash, copy completes or errors
#    cleanly (bounded by a 5-min cap; success = no crash/hang).
# S5 corrupt-image: feed apfs-mount a deliberately truncated/garbled image
#    via a temp file device path is N/A on raw; instead point apfs-mount
#    at a non-APFS PhysicalDrive number -> assert: clean
#    "mount_apfs: ... failed" message + exit 1, NO panic/crash.
# Each scenario must restore state (online disk, kill stray procs) so the
# next scenario starts clean; the script ALWAYS cleans up at the end.
```

(The implementer writes the concrete PowerShell. Use `diskpart` offline/online or `Disable-PnpDevice`/`Enable-PnpDevice` on the USB instance for S1/S3; wrap risky waits in `Start-Job` + `Wait-Job -Timeout` so a hang is DETECTED, never hangs SSH; always `Stop-Process apfs-tray,apfs-mount -Force` + bring the disk back online in a finally block.)

- [ ] **Step 2: Run the matrix on the box**

Sync + run `scripts/robustness.ps1` over SSH (hang-safe wrapper). Capture PASS/FAIL per scenario. Requirement to pass M6: S1, S2, S3, S5 PASS (no panic/hang/orphan; clean errors/unmount; agent survives); S4 PASS = no crash/hang within the cap. Any FAIL → root-cause in `apfs-tray`/`winfsp-host` (NOT apfs-core/apfs/FsView readers, which are proven), fix on the Mac, re-sync, re-run until green. Do NOT weaken assertions to make it pass.

- [ ] **Step 3: Commit the script (+ any fixes under their own messages)**

```bash
git add scripts/robustness.ps1
git commit -m "test(robustness): real-USB chaos scenario matrix (surprise-removal, agent-kill, rapid-replug, slow, corrupt)"
```

---

### Task 7: Final review + ROADMAP + finish

**Files:**
- Modify: `docs/ROADMAP.md`

- [ ] **Step 1: Final consolidated independent review** of the whole `feat/m6-packaging-msi` branch: spec-compliance (one-installer chains WinFsp+MSI, autostart, both exes; robustness matrix real & assertions not weakened) + quality + gates (`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace` on macOS; `installer` unit tests; WiX XML well-formed). Confirm reader crates untouched. Fix blocking findings.

- [ ] **Step 2: Update `docs/ROADMAP.md`** — M6 status row + squash commit; "Compact sonrası DEVAM" next = **M7** (apfs-write: snapshot→write→re-read→checksum→rollback); durable M6 notes (resolved WiX/cargo-wix flags, WinFsp MSI version + registry marker, bundle skip-if-present logic, signing step); robustness matrix results + that write-crash=M7 / fuzz+power-loss=M8 (#50). Re-index `project:roadmap-state`.

- [ ] **Step 3: Commit + finish**

```bash
git add docs/ROADMAP.md
git commit -m "docs(roadmap): M6 one-installer + robustness pass complete; next M7"
```

Then superpowers:finishing-a-development-branch: verify gates → squash-merge `feat/m6-packaging-msi` → main → delete branch → fill `<squash>` SHA → re-index KB.

---

## Self-Review

**Spec coverage:** ROADMAP M6 = WiX/MSI + WinFsp bundled + autostart + signing → Tasks 2 (MSI), 3 (Burn bundle chaining WinFsp), 2/3 (HKCU autostart), 4/5 (build driver + signtool), 5 (box install/uninstall proof). User's robustness concern (#50 realistic subset) = Task 6 first-class on real hardware with explicit invariants. No APFS on-disk spec → no md.6. Write-crash deferred to M7, fuzz/power-loss/soak to M8 (explicit, tracked in #50).

**Placeholder scan:** every code/XML step is concrete. GUIDs are intentionally generated in a named step (Task 2/3 Step 2), not left as literal placeholders in the final artifact. Exact WiX/cargo-wix flag shapes are framed as box-toolset-arbiter with a concrete resolution procedure (Task 5 Step 2) — the project's empirical-arbiter methodology, not a placeholder. Task 6's PowerShell body is specified by an explicit scenario contract (S1–S5 with exact assertions and the hang-safe/cleanup requirement) rather than verbatim script, because the precise diskpart/PnP commands are resolved against the box; the invariants and structure are fully pinned.

**Type consistency:** `installer::installer_version(&str) -> String` used only by the (future) xtask `-dVersion=`; `xtask package` subcommand name matches the dispatch note; WiX `var.TrayExe`/`var.MountExe`/`var.Version` are passed by the xtask `-C -d...` flags; bundle chains `dist/winfsp.msi` then `dist/apfs-explorer.msi` consistently; HKCU Run value points at `[INSTALLFOLDER]apfs-tray.exe` which Task 5 verifies.

**Scope:** one milestone; pure logic (version derivation) macOS-tested; packaging + robustness proven on real hardware; lean consolidated review. Robustness invariants are non-negotiable and explicitly must not be weakened to pass.
