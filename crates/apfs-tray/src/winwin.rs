//! Hidden message-only window that receives WM_DEVICECHANGE for volume
//! arrival/removal, so the agent reacts to USB plug/unplug. Windows-only.

use std::sync::mpsc::Sender;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    // windows 0.58: RegisterClassW/WNDCLASSW require Win32_Graphics_Gdi;
    // RegisterClassExW/WNDCLASSEXW are ungated — use Ex variants instead.
    CreateWindowExW,
    DefWindowProcW,
    RegisterClassExW,
    HWND_MESSAGE,
    WINDOW_EX_STYLE,
    WINDOW_STYLE,
    WM_DEVICECHANGE,
    WNDCLASSEXW,
};

/// Sent on the channel when any device-change event arrives; the main loop
/// then re-scans drives (debounced) — we don't parse the broadcast struct,
/// a re-scan via `detect` is simpler and robust.
pub struct DeviceEvent;

static mut TX: Option<Sender<DeviceEvent>> = None;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_DEVICECHANGE {
        // Any device-change: notify the loop to re-scan (debounced there).
        #[allow(static_mut_refs)]
        if let Some(tx) = TX.as_ref() {
            let _ = tx.send(DeviceEvent);
        }
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// Create the hidden message-only window. Returns its HWND. Sends a
/// `DeviceEvent` on `tx` for every WM_DEVICECHANGE.
pub fn create_device_window(tx: Sender<DeviceEvent>) -> windows::core::Result<HWND> {
    unsafe {
        // SAFETY: single-threaded init before the message loop starts;
        // wndproc only reads TX after this is set.
        TX = Some(tx);
        let hinst = GetModuleHandleW(PCWSTR::null())?;
        // windows 0.58: HMODULE and HINSTANCE share the same inner *mut c_void layout.
        let hinstance = HINSTANCE(hinst.0);
        let class_name = windows::core::w!("ApfsTrayDevWnd");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        // windows 0.58: CreateWindowExW parent param is Param<HWND>; pass the
        // const HWND directly (not Option<HWND>) to avoid windows_core version
        // conflict from the winfsp transitive dep (windows_core 0.61 vs 0.58).
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            windows::core::w!("apfs-tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            hinstance,
            None,
        )?;
        Ok(hwnd)
    }
}
