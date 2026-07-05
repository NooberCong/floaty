//! "Water color" popup: a hue slider with a live preview of the shallow→deep
//! gradient. Dragging posts WM_WATER_PREVIEW to the owner so the overlay
//! recolors immediately; nothing is written to config until OK posts
//! WM_WATER_COMMIT. "Reset to current" snaps back to the saved color and
//! Cancel / closing the window posts WM_WATER_CANCEL to revert the preview.

use anyhow::{Context, Result};
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::{parse_color, Config};

/// Posted to the owner while dragging; lparam = hue in tenths of a degree,
/// wparam = 1 when the base palette is the stock default (after "Reset to
/// default") instead of the saved config colors.
pub const WM_WATER_PREVIEW: u32 = WM_APP + 2;
/// Posted to the owner when OK is clicked; the previewed color should be saved.
pub const WM_WATER_COMMIT: u32 = WM_APP + 3;
/// Posted to the owner on Cancel/close; the preview should be discarded.
pub const WM_WATER_CANCEL: u32 = WM_APP + 4;

const ID_OK: u32 = 1;
const ID_CANCEL: u32 = 2;
const ID_RESET: u32 = 3;

// Layout in 96-dpi units, scaled at runtime.
const CLIENT_W: i32 = 340;
const CLIENT_H: i32 = 142;
const MARGIN: i32 = 14;
const BAR_Y: i32 = 14;
const BAR_H: i32 = 22;
const SWATCH_Y: i32 = 46;
const SWATCH_H: i32 = 24;
const HINT_Y: i32 = 76;
const HINT_H: i32 = 18;
const BTN_Y: i32 = 102;
const BTN_H: i32 = 26;

struct State {
    owner: HWND,
    shallow0: [f32; 3],
    deep0: [f32; 3],
    /// Hue of the base shallow color — the slider position that reproduces
    /// the base palette exactly.
    hue0: f32,
    hue: f32,
    /// True after "Reset to default": the base palette is the stock colors.
    use_stock: bool,
    scale: f32,
    dragging: bool,
    /// Set once OK or Cancel has posted its message, so WM_DESTROY does not
    /// post a second (cancelling) verdict.
    decided: bool,
}

/// Open the picker near the cursor. `owner` receives the WM_WATER_* messages.
pub fn open(owner: HWND, shallow_hex: &str, deep_hex: &str) -> Result<HWND> {
    let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW")?;

    static REGISTER: std::sync::Once = std::sync::Once::new();
    REGISTER.call_once(|| unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: w!("FloatyColorPicker"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        RegisterClassW(&class);
    });

    let shallow0 = parse_color(shallow_hex);
    let deep0 = parse_color(deep_hex);
    let hue0 = rgb_to_hsl(shallow0).0;
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;

    let state = Box::new(State {
        owner,
        shallow0,
        deep0,
        hue0,
        hue: hue0,
        use_stock: false,
        scale,
        dragging: false,
        decided: false,
    });

    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let exstyle = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: sc(CLIENT_W, scale),
        bottom: sc(CLIENT_H, scale),
    };
    unsafe { AdjustWindowRectEx(&mut rect, style, false, exstyle) }.context("AdjustWindowRectEx")?;
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);

    // Above the cursor (which is usually on the taskbar), clamped to screen.
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let x = (pt.x - w / 2).clamp(8, (sw - w - 8).max(8));
    let y = if pt.y - h - 16 > 0 { pt.y - h - 16 } else { (pt.y + 16).min(sh - h) };

    let hwnd = unsafe {
        CreateWindowExW(
            exstyle,
            w!("FloatyColorPicker"),
            w!("Water color"),
            style,
            x,
            y,
            w,
            h,
            Some(owner),
            None,
            Some(hinstance.into()),
            Some(Box::into_raw(state) as *const core::ffi::c_void),
        )
    }
    .context("creating color picker window")?;

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
    Ok(hwnd)
}

fn sc(v: i32, scale: f32) -> i32 {
    (v as f32 * scale).round() as i32
}

fn bar_rect(scale: f32) -> RECT {
    RECT {
        left: sc(MARGIN, scale),
        top: sc(BAR_Y, scale),
        right: sc(CLIENT_W - MARGIN, scale),
        bottom: sc(BAR_Y + BAR_H, scale),
    }
}

fn state_of(hwnd: HWND) -> *mut State {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CREATE {
        let cs = lparam.0 as *const CREATESTRUCTW;
        let ptr = unsafe { (*cs).lpCreateParams } as *mut State;
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            create_buttons(hwnd, (*ptr).scale);
        }
        return LRESULT(0);
    }

    let ptr = state_of(hwnd);
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let st = unsafe { &mut *ptr };

    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd, st);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = (loword_i(lparam), hiword_i(lparam));
            let bar = bar_rect(st.scale);
            if x >= bar.left && x <= bar.right && y >= bar.top - 6 && y <= bar.bottom + 6 {
                st.dragging = true;
                unsafe {
                    SetCapture(hwnd);
                }
                slide_to(hwnd, st, x);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if st.dragging => {
            slide_to(hwnd, st, loword_i(lparam));
            LRESULT(0)
        }
        WM_LBUTTONUP if st.dragging => {
            st.dragging = false;
            unsafe {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            match wparam.0 as u16 {
                k if k == VK_ESCAPE.0 => finish(hwnd, st, false),
                k if k == VK_RETURN.0 => finish(hwnd, st, true),
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wparam.0 & 0xffff) as u32 {
                ID_OK => finish(hwnd, st, true),
                ID_CANCEL => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                ID_RESET => {
                    // Back to the stock Floaty blue; further drags recolor
                    // the stock palette rather than the saved one.
                    let def = Config::default();
                    st.shallow0 = parse_color(&def.water_shallow);
                    st.deep0 = parse_color(&def.water_deep);
                    st.hue0 = rgb_to_hsl(st.shallow0).0;
                    st.hue = st.hue0;
                    st.use_stock = true;
                    post_preview(st);
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if !st.decided {
                unsafe {
                    let _ = PostMessageW(Some(st.owner), WM_WATER_CANCEL, WPARAM(0), LPARAM(0));
                }
            }
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn create_buttons(hwnd: HWND, scale: f32) {
    let hinstance = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    let make = |label: windows::core::PCWSTR, id: u32, x: i32, w: i32, default: bool| {
        let style = WS_CHILD
            | WS_VISIBLE
            | WINDOW_STYLE(if default { BS_DEFPUSHBUTTON } else { BS_PUSHBUTTON } as u32);
        if let Ok(btn) = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                label,
                style,
                sc(x, scale),
                sc(BTN_Y, scale),
                sc(w, scale),
                sc(BTN_H, scale),
                Some(hwnd),
                Some(HMENU(id as usize as *mut core::ffi::c_void)),
                Some(hinstance.into()),
                None,
            )
        } {
            unsafe {
                SendMessageW(btn, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
            }
        }
    };
    make(w!("Reset to default"), ID_RESET, MARGIN, 118, false);
    make(w!("OK"), ID_OK, CLIENT_W - MARGIN - 72 - 8 - 72, 72, true);
    make(w!("Cancel"), ID_CANCEL, CLIENT_W - MARGIN - 72, 72, false);
}

fn loword_i(lparam: LPARAM) -> i32 {
    (lparam.0 & 0xffff) as i16 as i32
}
fn hiword_i(lparam: LPARAM) -> i32 {
    ((lparam.0 >> 16) & 0xffff) as i16 as i32
}

fn slide_to(hwnd: HWND, st: &mut State, x: i32) {
    let bar = bar_rect(st.scale);
    let t = ((x - bar.left) as f32 / (bar.right - bar.left).max(1) as f32).clamp(0.0, 1.0);
    st.hue = t * 360.0;
    post_preview(st);
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn post_preview(st: &State) {
    unsafe {
        let _ = PostMessageW(
            Some(st.owner),
            WM_WATER_PREVIEW,
            WPARAM(st.use_stock as usize),
            LPARAM((st.hue * 10.0).round() as isize),
        );
    }
}

fn finish(hwnd: HWND, st: &mut State, commit: bool) {
    st.decided = true;
    let msg = if commit { WM_WATER_COMMIT } else { WM_WATER_CANCEL };
    unsafe {
        let _ = PostMessageW(Some(st.owner), msg, WPARAM(0), LPARAM(0));
        let _ = DestroyWindow(hwnd);
    }
}

// ---------------------------------------------------------------- paint ----

fn paint(hwnd: HWND, st: &State) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right, rc.bottom);

        // Double-buffer: the swatch repaints on every drag tick.
        let mem = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old = SelectObject(mem, bmp.into());

        FillRect(mem, &rc, GetSysColorBrush(COLOR_3DFACE));
        let dc_brush = HBRUSH(GetStockObject(DC_BRUSH).0);
        let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);

        // Hue bar.
        let bar = bar_rect(st.scale);
        for x in bar.left..bar.right {
            let hue = (x - bar.left) as f32 / (bar.right - bar.left) as f32 * 360.0;
            SetDCBrushColor(mem, colorref(hsl_to_rgb(hue, 0.85, 0.55)));
            let col = RECT { left: x, top: bar.top, right: x + 1, bottom: bar.bottom };
            FillRect(mem, &col, dc_brush);
        }
        FrameRect(mem, &bar, black);

        // Thumb.
        let tx = bar.left + ((st.hue / 360.0) * (bar.right - bar.left) as f32) as i32;
        let thumb = RECT {
            left: tx - 3,
            top: bar.top - 4,
            right: tx + 4,
            bottom: bar.bottom + 4,
        };
        SetDCBrushColor(mem, COLORREF(0x00ffffff));
        FillRect(mem, &thumb, dc_brush);
        FrameRect(mem, &thumb, black);

        // Preview swatch: the shallow→deep gradient this hue produces.
        let (shallow, deep) = derive_rgb(st.shallow0, st.deep0, st.hue);
        let sw = RECT {
            left: sc(MARGIN, st.scale),
            top: sc(SWATCH_Y, st.scale),
            right: sc(CLIENT_W - MARGIN, st.scale),
            bottom: sc(SWATCH_Y + SWATCH_H, st.scale),
        };
        for y in sw.top..sw.bottom {
            let t = (y - sw.top) as f32 / (sw.bottom - sw.top).max(1) as f32;
            let c = [
                shallow[0] + (deep[0] - shallow[0]) * t,
                shallow[1] + (deep[1] - shallow[1]) * t,
                shallow[2] + (deep[2] - shallow[2]) * t,
            ];
            SetDCBrushColor(mem, colorref(c));
            let row = RECT { left: sw.left, top: y, right: sw.right, bottom: y + 1 };
            FillRect(mem, &row, dc_brush);
        }
        FrameRect(mem, &sw, black);

        // Hint line.
        let font_old = SelectObject(mem, GetStockObject(DEFAULT_GUI_FONT));
        SetBkMode(mem, TRANSPARENT);
        SetTextColor(mem, COLORREF(0x00707070));
        let mut text: Vec<u16> =
            "Live preview on the taskbar \u{2014} OK applies, Cancel puts it back."
                .encode_utf16()
                .collect();
        let mut trc = RECT {
            left: sc(MARGIN, st.scale),
            top: sc(HINT_Y, st.scale),
            right: sc(CLIENT_W - MARGIN, st.scale),
            bottom: sc(HINT_Y + HINT_H, st.scale),
        };
        DrawTextW(mem, &mut text, &mut trc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        SelectObject(mem, font_old);

        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn colorref(c: [f32; 3]) -> COLORREF {
    let r = (c[0].clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (c[1].clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (c[2].clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF(r | (g << 8) | (b << 16))
}

// ---------------------------------------------------------------- color ----

/// Recolor the saved shallow/deep pair to `hue`: each keeps its saturation and
/// lightness, and deep keeps its hue offset relative to shallow, so the pair
/// stays a coherent "water" gradient. At the saved color's own hue the inputs
/// are returned untouched, so Reset → OK is an exact no-op.
pub fn derive(shallow_hex: &str, deep_hex: &str, hue: f32) -> (String, String) {
    let (s, d) = derive_rgb(parse_color(shallow_hex), parse_color(deep_hex), hue);
    (hex(s), hex(d))
}

fn derive_rgb(shallow0: [f32; 3], deep0: [f32; 3], hue: f32) -> ([f32; 3], [f32; 3]) {
    let (h_s, mut s_s, l_s) = rgb_to_hsl(shallow0);
    let (h_d, mut s_d, l_d) = rgb_to_hsl(deep0);
    if hue_dist(hue, h_s) < 0.5 {
        return (shallow0, deep0);
    }
    // A grey base would make the slider a no-op; give it some body to color.
    if s_s < 0.05 {
        s_s = 0.6;
    }
    if s_d < 0.05 {
        s_d = 0.6;
    }
    let delta = hue - h_s;
    (
        hsl_to_rgb(wrap_hue(hue), s_s, l_s),
        hsl_to_rgb(wrap_hue(h_d + delta), s_d, l_d),
    )
}

fn wrap_hue(h: f32) -> f32 {
    h.rem_euclid(360.0)
}

fn hue_dist(a: f32, b: f32) -> f32 {
    let d = (wrap_hue(a) - wrap_hue(b)).abs();
    d.min(360.0 - d)
}

fn hex(c: [f32; 3]) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[2].clamp(0.0, 1.0) * 255.0).round() as u8
    )
}

fn rgb_to_hsl(c: [f32; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (c[0], c[1], c[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs()).max(1e-6);
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h, s.clamp(0.0, 1.0), l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let h = wrap_hue(h);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c * 0.5;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}
