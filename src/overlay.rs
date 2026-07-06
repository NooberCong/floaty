//! The transparent, click-through overlay window that sits exactly on top of
//! a taskbar. All input passes through to the real taskbar underneath; the
//! window contributes pixels only via its DirectComposition visual.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS_NAME: windows::core::PCWSTR = w!("FloatyOverlay");

pub fn register_class() -> Result<()> {
    let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW")?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(overlay_proc),
        hInstance: hinstance.into(),
        lpszClassName: CLASS_NAME,
        hbrBackground: HBRUSH::default(), // never painted; DComp owns the pixels
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        anyhow::bail!("RegisterClassW failed: {:?}", windows::core::Error::from_thread());
    }
    Ok(())
}

pub fn create(x: i32, y: i32, w: i32, h: i32) -> Result<HWND> {
    let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW")?;
    let hwnd = unsafe {
        CreateWindowExW(
            // NOREDIRECTIONBITMAP: pure composition content, no GDI surface.
            // LAYERED+TRANSPARENT: full click-through; NOACTIVATE+TOOLWINDOW:
            // never steals focus, never appears in alt-tab.
            WS_EX_NOREDIRECTIONBITMAP
                | WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE
                | WS_EX_TOPMOST,
            CLASS_NAME,
            w!("Floaty"),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .context("CreateWindowExW overlay")?;

    unsafe {
        // A layered window only renders once its attributes are set.
        SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 255, LWA_ALPHA)
            .context("SetLayeredWindowAttributes")?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(hwnd)
}

/// Keep the overlay glued to the taskbar without activating or resizing churn.
pub fn place(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> Result<()> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
        .context("SetWindowPos overlay")
    }
}

pub fn destroy(hwnd: HWND) {
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Set by the win-event hook whenever the shell reshuffles top-level windows
/// (clicking the taskbar or opening its flyouts hoists the bar above every
/// topmost window, covering us). The main loop consumes it via
/// [`take_raise_request`] and re-raises immediately instead of waiting for
/// the 1 Hz geometry poll.
static RAISE_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn raise_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    idobject: i32,
    _idchild: i32,
    _id_thread: u32,
    _time: u32,
) {
    // Whole-window changes only; child-object churn (list items, captions,
    // etc.) can't affect top-level z-order.
    if idobject == OBJID_WINDOW.0 {
        RAISE_REQUESTED.store(true, Ordering::Relaxed);
    }
}

/// Watch for the z-order changes that can put the taskbar above the overlay:
/// foreground switches (taskbar clicks, app switches, shell flyouts) and
/// top-level reorders that don't change activation.
///
/// Must be called on the thread that pumps messages: WINEVENT_OUTOFCONTEXT
/// callbacks are delivered through the installing thread's message loop,
/// which also wakes that loop the moment an event arrives.
/// SKIPOWNPROCESS keeps our own SetWindowPos calls from re-triggering the
/// hook. The hooks intentionally live until process exit, when Windows
/// removes them — no unhook bookkeeping.
pub fn install_raise_hooks() {
    for event in [EVENT_SYSTEM_FOREGROUND, EVENT_OBJECT_REORDER] {
        let hook = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(raise_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            // Not fatal: the slow poll still reasserts z-order every second.
            log::warn!("SetWinEventHook({event:#x}) failed; overlay may briefly hide behind the taskbar");
        }
    }
}

/// Consume a pending raise request from the win-event hook. Coalesces event
/// bursts: any number of events between two polls costs one raise.
pub fn take_raise_request() -> bool {
    RAISE_REQUESTED.swap(false, Ordering::Relaxed)
}

/// Z-order-only reassert, cheap enough to run on every shell reorder. No
/// SWP_SHOWWINDOW, so overlays hidden for unusable bars stay hidden.
pub fn raise(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Belt and braces on top of WS_EX_TRANSPARENT: report every point as
        // transparent so hit-testing can never land on us.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
