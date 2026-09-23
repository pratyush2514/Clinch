//! Best-effort OS hiding for the off-screen headed browser (Windows only).
//!
//! The off-screen window never paints on a monitor (far-positive
//! coordinates), but Windows still lists a taskbar button for any visible
//! top-level window. This module hides those windows via direct Win32
//! calls. Raw FFI needs `unsafe`, which the crate denies at its root; the
//! `allow` below is scoped to this module, and each unsafe block documents
//! its contract. No new dependencies: the four functions used are declared
//! by hand.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::os::raw::{c_int, c_void};

type HWND = *mut c_void;
type BOOL = c_int;
type DWORD = u32;
type LPARAM = isize;

unsafe extern "system" {
    fn EnumWindows(
        lp_enum_func: unsafe extern "system" fn(HWND, LPARAM) -> BOOL,
        l_param: LPARAM,
    ) -> BOOL;
    fn GetWindowThreadProcessId(h_wnd: HWND, lpdw_process_id: *mut DWORD) -> DWORD;
    fn IsWindowVisible(h_wnd: HWND) -> BOOL;
    fn ShowWindow(h_wnd: HWND, n_cmd_show: c_int) -> BOOL;
}

const SW_HIDE: c_int = 0;
const TRUE: BOOL = 1;

struct HideContext {
    pid: DWORD,
    hidden: u32,
}

/// `EnumWindows` callback: hide visible top-level windows owned by our pid.
/// The `l_param` is always the `HideContext` this module passed to
/// `EnumWindows`; HWNDs are acted on immediately and never stored.
unsafe extern "system" fn hide_callback(h_wnd: HWND, l_param: LPARAM) -> BOOL {
    let ctx = unsafe { &mut *(l_param as *mut HideContext) };
    let mut pid: DWORD = 0;
    unsafe {
        GetWindowThreadProcessId(h_wnd, &mut pid);
        if pid == ctx.pid && IsWindowVisible(h_wnd) == TRUE {
            ShowWindow(h_wnd, SW_HIDE);
            ctx.hidden += 1;
        }
    }
    TRUE
}

/// Hide visible top-level windows owned by `pid`, retrying briefly: the
/// browser window typically appears within a few hundred ms of spawn.
/// Returns the number hidden. Best-effort by design — a zero just leaves
/// the off-screen (but taskbar-listed) window where it is.
pub fn hide_process_windows(pid: u32) -> u32 {
    let mut ctx = HideContext { pid, hidden: 0 };
    for _ in 0..20 {
        // SAFETY: `hide_callback` only dereferences the context pointer for
        // the duration of this call; nothing escapes.
        unsafe {
            EnumWindows(hide_callback, &mut ctx as *mut HideContext as LPARAM);
        }
        if ctx.hidden > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    ctx.hidden
}
