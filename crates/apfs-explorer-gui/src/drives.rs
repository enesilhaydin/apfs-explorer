//! Windows physical-disk enumeration via WMI (`Win32_DiskDrive`).
//!
//! WMI is queryable by any user without admin privileges - perfect for the
//! GUI's "list candidate drives" step.  We surface every physical disk and
//! let the user pick one to try mounting; the actual APFS detection happens
//! inside `apfs-mount.exe` (which runs elevated, can open the raw device,
//! and returns an error if the disk is not APFS).

use std::collections::HashSet;
use std::sync::Mutex;

use crate::commands::DriveInfo;

/// Apple_APFS GPT partition type GUID. A disk is "APFS" iff one of its
/// `MSFT_Partition` rows has this `GptType`.
const APFS_GPT_TYPE: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";

use windows::core::BSTR;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{
    VariantChangeType, VariantClear, VARIANT, VAR_CHANGE_FLAGS, VT_BSTR,
};
use windows::Win32::System::Wmi::{
    IWbemLocator, WbemLocator, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
};

/// Tracks which drives the current GUI process has mounted.  Populated by
/// `mount::mount()`; read by `list()` so the UI can mark them.
pub static MOUNTED: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());

#[derive(Debug, thiserror::Error)]
pub enum DriveError {
    #[error("WMI init failed: {0}")]
    Init(String),
    #[error("WMI query failed: {0}")]
    Query(String),
}

/// Enumerate physical disks on the local machine.
///
/// Returns one entry per `Win32_DiskDrive`.  Mounted state is filled from the
/// in-process `MOUNTED` registry so the UI can render the right action button.
///
/// The WMI call runs on a freshly-spawned `std::thread` so we control the COM
/// apartment model (MTA, our preference) without colliding with whatever
/// Tauri's webview thread has initialised on the calling thread.  The thread
/// joins quickly (~50 ms) so blocking the async handler is fine.
pub fn list() -> Result<Vec<DriveInfo>, DriveError> {
    tracing::debug!("list_drives: entering");
    // Re-discover drives still mounted by the launcher (e.g. after a GUI
    // restart) and prune any unmounted out-of-band, before reading MOUNTED.
    crate::mount::reconcile();
    let join = std::thread::spawn(|| {
        tracing::debug!("list_drives: WMI worker thread start");
        let r = wmi_query_disk_drives();
        tracing::debug!("list_drives: WMI worker returned ok={}", r.is_ok());
        r
    });
    let (raw, apfs) = match join.join() {
        Ok(r) => r?,
        Err(_) => return Err(DriveError::Query("WMI worker thread panicked".into())),
    };
    tracing::debug!(
        "list_drives: got {} raw drives, apfs detection={}",
        raw.len(),
        apfs.as_ref()
            .map_or("unavailable (fail-open)".into(), |s| format!(
                "{} APFS disk(s)",
                s.len()
            )),
    );

    let mounted = MOUNTED.lock().map(|g| g.clone()).unwrap_or_default();
    let drives = raw
        .into_iter()
        .map(|d| {
            let m = mounted.iter().find(|(idx, _)| *idx == d.index);
            // Fail open: if APFS detection was unavailable (None), treat every
            // disk as APFS so a Storage-WMI failure never hides all drives.
            let is_apfs = apfs.as_ref().map_or(true, |s| s.contains(&d.index));
            DriveInfo {
                index: d.index,
                model: d.model,
                size_bytes: d.size_bytes,
                kind: d.kind,
                is_apfs,
                mounted: m.is_some(),
                letter: m.map(|(_, l)| l.clone()).unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    tracing::debug!("list_drives: returning {} drives", drives.len());
    Ok(drives)
}

struct RawDrive {
    index: u32,
    model: String,
    size_bytes: u64,
    kind: String,
}

/// Pull `Index`, `Model`, `Size`, `MediaType`, `InterfaceType` from every
/// `Win32_DiskDrive`.  Uses COM + WMI which is the standard non-admin API
/// surface for hardware enumeration on Windows.
fn wmi_query_disk_drives() -> Result<(Vec<RawDrive>, Option<HashSet<u32>>), DriveError> {
    unsafe {
        tracing::debug!("wmi: CoInitializeEx(MTA)");
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        tracing::debug!("wmi: CoInitializeEx returned hr={:#x}", hr.0);
        // -2147417850 == RPC_E_CHANGED_MODE: COM already initialised on
        // this thread (shouldn't happen on a fresh std::thread, but be
        // defensive in case).  We continue either way and only uninit at
        // the end when we performed the init ourselves.
        let owns_com_init = hr.is_ok();

        tracing::debug!("wmi: entering inner");
        let drives = wmi_query_inner();
        tracing::debug!("wmi: inner returned ok={}", drives.is_ok());

        // APFS detection is best-effort: query the Storage namespace for
        // partition GptTypes. On ANY failure return None (fail open) rather
        // than an empty set, so detection trouble never hides every disk.
        let apfs = match wmi_apfs_inner() {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("wmi: APFS partition detection unavailable: {e}");
                None
            }
        };

        if owns_com_init {
            CoUninitialize();
        }
        drives.map(|d| (d, apfs))
    }
}

/// Set of physical-disk numbers that carry an Apple_APFS GPT partition.
/// Queries `MSFT_Partition` in the Storage WMI namespace (read-only, works for
/// standard users - no elevation, no raw-disk access).
unsafe fn wmi_apfs_inner() -> Result<HashSet<u32>, DriveError> {
    let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
        .map_err(|e| DriveError::Init(format!("CoCreateInstance(WbemLocator)[storage]: {e}")))?;
    let services = locator
        .ConnectServer(
            &BSTR::from(r"ROOT\Microsoft\Windows\Storage"),
            &BSTR::new(),
            &BSTR::new(),
            &BSTR::new(),
            0,
            &BSTR::new(),
            None,
        )
        .map_err(|e| DriveError::Init(format!("ConnectServer[storage]: {e}")))?;
    CoSetProxyBlanket(
        &services,
        RPC_C_AUTHN_WINNT,
        RPC_C_AUTHZ_NONE,
        None,
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    )
    .map_err(|e| DriveError::Init(format!("CoSetProxyBlanket[storage]: {e}")))?;

    let query = services
        .ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT DiskNumber, GptType FROM MSFT_Partition"),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
        .map_err(|e| DriveError::Query(format!("ExecQuery[MSFT_Partition]: {e}")))?;

    let want = APFS_GPT_TYPE.trim_matches(|c| c == '{' || c == '}');
    let mut set: HashSet<u32> = HashSet::new();
    loop {
        let mut row = [None];
        let mut returned: u32 = 0;
        let hr = query.Next(2_000, &mut row, &mut returned);
        if hr.is_err() || returned == 0 {
            break;
        }
        let Some(obj) = row[0].take() else {
            break;
        };
        let gpt = wmi_get_string(&obj, "GptType").unwrap_or_default();
        let gpt = gpt.trim().trim_matches(|c| c == '{' || c == '}');
        if gpt.eq_ignore_ascii_case(want) {
            if let Some(n) = wmi_get_u32(&obj, "DiskNumber") {
                set.insert(n);
            }
        }
    }
    Ok(set)
}

unsafe fn wmi_query_inner() -> Result<Vec<RawDrive>, DriveError> {
    tracing::debug!("wmi: CoCreateInstance(WbemLocator)");
    let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
        .map_err(|e| DriveError::Init(format!("CoCreateInstance(WbemLocator): {e}")))?;
    tracing::debug!("wmi: ConnectServer(ROOT\\CIMV2)");

    let services = locator
        .ConnectServer(
            &BSTR::from(r"ROOT\CIMV2"),
            &BSTR::new(),
            &BSTR::new(),
            &BSTR::new(),
            0,
            &BSTR::new(),
            None,
        )
        .map_err(|e| DriveError::Init(format!("ConnectServer: {e}")))?;

    tracing::debug!("wmi: CoSetProxyBlanket");
    CoSetProxyBlanket(
        &services,
        RPC_C_AUTHN_WINNT,
        RPC_C_AUTHZ_NONE,
        None,
        RPC_C_AUTHN_LEVEL_CALL,
        RPC_C_IMP_LEVEL_IMPERSONATE,
        None,
        EOAC_NONE,
    )
    .map_err(|e| DriveError::Init(format!("CoSetProxyBlanket: {e}")))?;

    tracing::debug!("wmi: ExecQuery");
    let query = services
        .ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from("SELECT Index, Model, Size, MediaType, InterfaceType FROM Win32_DiskDrive"),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
        .map_err(|e| DriveError::Query(format!("ExecQuery: {e}")))?;
    tracing::debug!("wmi: ExecQuery ok, iterating");

    let mut drives: Vec<RawDrive> = Vec::new();
    let mut iters = 0u32;
    loop {
        iters += 1;
        let mut row = [None];
        let mut returned: u32 = 0;
        // 2-second timeout per row instead of WBEM_INFINITE: WMI sometimes
        // hangs at end-of-set if the query has run out of rows and was not
        // closed cleanly.  After the timeout we treat the iteration as
        // done.
        let hr = query.Next(2_000, &mut row, &mut returned);
        tracing::debug!("wmi: Next iter={iters} hr={:#x} returned={returned}", hr.0);
        if hr.is_err() || returned == 0 {
            break;
        }
        let Some(obj) = row[0].take() else {
            break;
        };

        let index = wmi_get_u32(&obj, "Index").unwrap_or(u32::MAX);
        let model = wmi_get_string(&obj, "Model").unwrap_or_default();
        let size_bytes = wmi_get_u64(&obj, "Size").unwrap_or(0);
        let media = wmi_get_string(&obj, "MediaType").unwrap_or_default();
        let interface_type = wmi_get_string(&obj, "InterfaceType").unwrap_or_default();
        let kind = classify_kind(&media, &interface_type);

        if index != u32::MAX {
            drives.push(RawDrive {
                index,
                model,
                size_bytes,
                kind,
            });
        }
    }

    drives.sort_by_key(|d| d.index);
    Ok(drives)
}

unsafe fn wmi_get_string(
    obj: &windows::Win32::System::Wmi::IWbemClassObject,
    name: &str,
) -> Option<String> {
    let mut v = VARIANT::default();
    let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let r = obj.Get(
        windows::core::PCWSTR(name_w.as_ptr()),
        0,
        &mut v,
        None,
        None,
    );
    if r.is_err() {
        return None;
    }
    let s = variant_to_string(&v);
    let _ = VariantClear(&mut v);
    s
}

unsafe fn wmi_get_u32(
    obj: &windows::Win32::System::Wmi::IWbemClassObject,
    name: &str,
) -> Option<u32> {
    wmi_get_string(obj, name).and_then(|s| s.parse().ok())
}

unsafe fn wmi_get_u64(
    obj: &windows::Win32::System::Wmi::IWbemClassObject,
    name: &str,
) -> Option<u64> {
    wmi_get_string(obj, name).and_then(|s| s.parse().ok())
}

/// Convert any VARIANT we get back from WMI into a plain `String`.  WMI
/// often returns numbers boxed as BSTR (the WBEM client is loosely-typed
/// over the wire), so we use VariantChangeType to coerce to BSTR uniformly.
unsafe fn variant_to_string(v: &VARIANT) -> Option<String> {
    let mut dst = VARIANT::default();
    if VariantChangeType(&mut dst, v, VAR_CHANGE_FLAGS(0), VT_BSTR).is_err() {
        return None;
    }
    // windows 0.61 VARIANT exposes the typed value via Debug + a typed
    // accessor.  The simplest portable way to extract the BSTR is via the
    // `to_string()` impl, which formats the contained typed value.  For a
    // freshly-coerced VT_BSTR variant this yields the actual string.
    let s = format!("{dst}");
    let _ = VariantClear(&mut dst);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn classify_kind(media: &str, interface_type: &str) -> String {
    let m = media.to_ascii_lowercase();
    let i = interface_type.to_ascii_lowercase();
    if i == "usb" || m.contains("removable") {
        "Removable".into()
    } else if m.contains("fixed") {
        "Fixed".into()
    } else if !i.is_empty() {
        capitalise(&interface_type.to_string())
    } else {
        "Unknown".into()
    }
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
