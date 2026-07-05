//! Locating taskbars and tracking their geometry. Windows exposes the primary
//! taskbar as the `Shell_TrayWnd` top-level window and one
//! `Shell_SecondaryTrayWnd` per additional monitor.

use windows::core::w;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, FindWindowW, GetWindowRect, IsWindowVisible};

#[derive(Clone, PartialEq, Debug)]
pub struct TaskbarInfo {
    pub hwnd: HWND,
    pub rect: RECT,
    pub visible: bool,
    pub primary: bool,
}

impl TaskbarInfo {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
    /// Overlaying only makes sense for horizontal taskbars of sane size
    /// (Windows 10 allows docking the bar vertically; skip those).
    pub fn usable(&self) -> bool {
        self.visible && self.width() > self.height() && self.height() >= 20 && self.height() <= 200
    }
}

/// Snapshot every taskbar currently present.
pub fn enumerate(include_secondary: bool) -> Vec<TaskbarInfo> {
    let mut bars = Vec::new();

    unsafe {
        if let Ok(hwnd) = FindWindowW(w!("Shell_TrayWnd"), None) {
            if let Some(info) = snapshot(hwnd, true) {
                bars.push(info);
            }
        }
        if include_secondary {
            let mut prev: Option<HWND> = None;
            loop {
                let Ok(hwnd) =
                    FindWindowExW(None, prev, w!("Shell_SecondaryTrayWnd"), None)
                else {
                    break;
                };
                if hwnd.is_invalid() {
                    break;
                }
                if let Some(info) = snapshot(hwnd, false) {
                    bars.push(info);
                }
                prev = Some(hwnd);
            }
        }
    }
    bars
}

fn snapshot(hwnd: HWND, primary: bool) -> Option<TaskbarInfo> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect).ok()?;
        let visible = IsWindowVisible(hwnd).as_bool();
        Some(TaskbarInfo { hwnd, rect, visible, primary })
    }
}
