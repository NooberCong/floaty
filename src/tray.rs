//! System tray icon and context menu. The menu is built fresh on each
//! right-click and returns a `TrayCommand` for the app to act on, keeping all
//! state ownership in `main`.

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::assets::ROSTER;

/// Private message the tray icon posts to our message window.
pub const WM_TRAYICON: u32 = WM_APP + 1;

const CMD_PAUSE: u32 = 1;
const CMD_MOUSE_RIPPLES: u32 = 2;
const CMD_AUTOSTART: u32 = 3;
const CMD_OPEN_CONFIG: u32 = 4;
const CMD_ABOUT: u32 = 5;
const CMD_EXIT: u32 = 6;
const CMD_CHARACTER_BASE: u32 = 100;
const CMD_CHARACTER_ROTATE: u32 = 198;
const CMD_CHARACTER_CUSTOM: u32 = 199;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    TogglePause,
    ToggleMouseRipples,
    ToggleAutostart,
    OpenConfig,
    About,
    Exit,
    SelectCharacter(String),
}

pub struct Tray {
    hwnd: HWND,
}

impl Tray {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW")?;
        // Icon resource id 1 (see res/floaty.rc). Ask for the exact tray icon
        // size so the shell picks the crisp small variant instead of scaling.
        let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
        let icon = unsafe {
            LoadImageW(
                Some(hinstance.into()),
                windows::core::PCWSTR(1 as _),
                IMAGE_ICON,
                cx,
                cy,
                LR_DEFAULTCOLOR,
            )
        }
        .map(|h| HICON(h.0))
        .unwrap_or_else(|_| unsafe { LoadIconW(None, IDI_APPLICATION).unwrap() });

        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: icon,
            ..Default::default()
        };
        let tip: Vec<u16> = "Floaty — taskbar pool\0".encode_utf16().collect();
        data.szTip[..tip.len()].copy_from_slice(&tip);

        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            anyhow::bail!("Shell_NotifyIconW(NIM_ADD) failed");
        }
        Ok(Self { hwnd })
    }

    /// Show the context menu at the cursor and translate the pick.
    pub fn show_menu(
        &self,
        paused: bool,
        mouse_ripples: bool,
        autostart: bool,
        character: &str,
        custom_available: bool,
    ) -> Option<TrayCommand> {
        unsafe {
            let menu = CreatePopupMenu().ok()?;

            let characters = CreatePopupMenu().ok()?;
            for (i, info) in ROSTER.iter().enumerate() {
                let checked = if character == info.id { MF_CHECKED } else { MF_UNCHECKED };
                let label: Vec<u16> = format!("{}\0", info.name).encode_utf16().collect();
                let _ = AppendMenuW(
                    characters,
                    MF_STRING | checked,
                    (CMD_CHARACTER_BASE + i as u32) as usize,
                    windows::core::PCWSTR(label.as_ptr()),
                );
            }
            let rotate_check = if character == "rotate" { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(
                characters,
                MF_STRING | rotate_check,
                CMD_CHARACTER_ROTATE as usize,
                w!("Surprise me (rotate)"),
            );
            let custom_flags = if custom_available { MF_STRING } else { MF_STRING | MF_GRAYED }
                | if character == "custom" { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(
                characters,
                custom_flags,
                CMD_CHARACTER_CUSTOM as usize,
                w!("Custom (from config)"),
            );

            let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let pause_label = if paused { w!("Resume") } else { w!("Pause") };
            let _ = AppendMenuW(menu, MF_STRING, CMD_PAUSE as usize, pause_label);
            let _ = AppendMenuW(menu, MF_POPUP, characters.0 as usize, w!("Character"));
            let _ = AppendMenuW(menu, MF_STRING | check(mouse_ripples), CMD_MOUSE_RIPPLES as usize, w!("Mouse ripples"));
            let _ = AppendMenuW(menu, MF_STRING | check(autostart), CMD_AUTOSTART as usize, w!("Start with Windows"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_CONFIG as usize, w!("Open settings file"));
            let _ = AppendMenuW(menu, MF_STRING, CMD_ABOUT as usize, w!("About Floaty"));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT as usize, w!("Exit"));

            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Required so the menu dismisses when clicking elsewhere.
            let _ = SetForegroundWindow(self.hwnd);
            let picked = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);

            let id = picked.0 as u32;
            match id {
                CMD_PAUSE => Some(TrayCommand::TogglePause),
                CMD_MOUSE_RIPPLES => Some(TrayCommand::ToggleMouseRipples),
                CMD_AUTOSTART => Some(TrayCommand::ToggleAutostart),
                CMD_OPEN_CONFIG => Some(TrayCommand::OpenConfig),
                CMD_ABOUT => Some(TrayCommand::About),
                CMD_EXIT => Some(TrayCommand::Exit),
                CMD_CHARACTER_ROTATE => Some(TrayCommand::SelectCharacter("rotate".into())),
                CMD_CHARACTER_CUSTOM => Some(TrayCommand::SelectCharacter("custom".into())),
                id if id >= CMD_CHARACTER_BASE => ROSTER
                    .get((id - CMD_CHARACTER_BASE) as usize)
                    .map(|c| TrayCommand::SelectCharacter(c.id.to_string())),
                _ => None,
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}
