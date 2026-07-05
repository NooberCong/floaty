//! The transparent, click-through overlay window that sits exactly on top of
//! a taskbar. All input passes through to the real taskbar underneath; the
//! window contributes pixels only via its DirectComposition visual.

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Belt and braces on top of WS_EX_TRANSPARENT: report every point as
        // transparent so hit-testing can never land on us.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
