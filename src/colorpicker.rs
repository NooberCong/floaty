//! "Water color" popup: hue / saturation / lightness sliders with a live
//! preview of the shallow→deep gradient. Dragging posts WM_WATER_PREVIEW to
//! the owner so the overlay recolors immediately; nothing is written to
//! config until OK posts
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

/// Posted to the owner while dragging; unpack the picked HSL and the
/// stock-palette flag with [`decode_preview`].
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
const CLIENT_H: i32 = 196;
const MARGIN: i32 = 14;
/// Width reserved left of the slider bars for their labels.
const LABEL_W: i32 = 62;
const BAR_Y: i32 = 14;
const BAR_H: i32 = 18;
/// Vertical distance between consecutive slider bar tops.
const BAR_STEP: i32 = 28;
const SWATCH_Y: i32 = 98;
const SWATCH_H: i32 = 24;
const HINT_Y: i32 = 130;
const HINT_H: i32 = 18;
const BTN_Y: i32 = 156;
const BTN_H: i32 = 26;

struct State {
    owner: HWND,
    shallow0: [f32; 3],
    deep0: [f32; 3],
    /// HSL of the base shallow color — the slider positions that reproduce
    /// the base palette exactly.
    hsl0: [f32; 3],
    /// Current slider values: hue in degrees, saturation / lightness in 0–1.
    hsl: [f32; 3],
    /// True after "Reset to default": the base palette is the stock colors.
    use_stock: bool,
    scale: f32,
    /// Index of the bar being dragged (0 = hue, 1 = saturation, 2 = lightness).
    dragging: Option<usize>,
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
    let (h, s, l) = rgb_to_hsl(shallow0);
    let scale = unsafe { GetDpiForSystem() } as f32 / 96.0;

    let state = Box::new(State {
        owner,
        shallow0,
        deep0,
        hsl0: [h, s, l],
        hsl: [h, s, l],
        use_stock: false,
        scale,
        dragging: None,
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

fn bar_rect(i: usize, scale: f32) -> RECT {
    let top = BAR_Y + i as i32 * BAR_STEP;
    RECT {
        left: sc(MARGIN + LABEL_W, scale),
        top: sc(top, scale),
        right: sc(CLIENT_W - MARGIN, scale),
        bottom: sc(top + BAR_H, scale),
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
            for i in 0..3 {
                let bar = bar_rect(i, st.scale);
                if x >= bar.left && x <= bar.right && y >= bar.top - 4 && y <= bar.bottom + 4 {
                    st.dragging = Some(i);
                    unsafe {
                        SetCapture(hwnd);
                    }
                    slide_to(hwnd, st, i, x);
                    break;
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(i) = st.dragging {
                slide_to(hwnd, st, i, loword_i(lparam));
            }
            LRESULT(0)
        }
        WM_LBUTTONUP if st.dragging.is_some() => {
            st.dragging = None;
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
                    let (h, s, l) = rgb_to_hsl(st.shallow0);
                    st.hsl0 = [h, s, l];
                    st.hsl = st.hsl0;
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

fn slide_to(hwnd: HWND, st: &mut State, i: usize, x: i32) {
    let bar = bar_rect(i, st.scale);
    let t = ((x - bar.left) as f32 / (bar.right - bar.left).max(1) as f32).clamp(0.0, 1.0);
    st.hsl[i] = if i == 0 { t * 360.0 } else { t };
    post_preview(st);
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn post_preview(st: &State) {
    let (wparam, lparam) = encode_preview(st.use_stock, st.hsl);
    unsafe {
        let _ = PostMessageW(Some(st.owner), WM_WATER_PREVIEW, wparam, lparam);
    }
}

/// Pack the WM_WATER_PREVIEW payload: lparam carries the hue in tenths of a
/// degree; wparam carries the stock-palette flag in bit 0 with saturation and
/// lightness in permille above it.
fn encode_preview(stock: bool, hsl: [f32; 3]) -> (WPARAM, LPARAM) {
    let s = (hsl[1] * 1000.0).round() as usize;
    let l = (hsl[2] * 1000.0).round() as usize;
    (
        WPARAM(stock as usize | (s << 1) | (l << 16)),
        LPARAM((hsl[0] * 10.0).round() as isize),
    )
}

/// Unpack a WM_WATER_PREVIEW payload into (use_stock, hue, saturation, lightness).
pub fn decode_preview(wparam: WPARAM, lparam: LPARAM) -> (bool, f32, f32, f32) {
    (
        wparam.0 & 1 != 0,
        lparam.0 as f32 / 10.0,
        ((wparam.0 >> 1) & 0x3ff) as f32 / 1000.0,
        ((wparam.0 >> 16) & 0x3ff) as f32 / 1000.0,
    )
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
        let font_old = SelectObject(mem, GetStockObject(DEFAULT_GUI_FONT));
        SetBkMode(mem, TRANSPARENT);

        // Slider bars. Hue is drawn at fixed vividness so the spectrum stays
        // readable; saturation and lightness are drawn at the current values
        // of the other channels, so each bar previews exactly what dragging
        // it would produce.
        let [hue, sat, light] = st.hsl;
        let bars = [("Hue", hue / 360.0), ("Saturation", sat), ("Lightness", light)];
        SetTextColor(mem, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
        for (i, (label, value)) in bars.into_iter().enumerate() {
            let bar = bar_rect(i, st.scale);
            let span = (bar.right - bar.left).max(1);

            let mut text: Vec<u16> = label.encode_utf16().collect();
            let mut lrc = RECT {
                left: sc(MARGIN, st.scale),
                top: bar.top,
                right: bar.left - sc(6, st.scale),
                bottom: bar.bottom,
            };
            DrawTextW(mem, &mut text, &mut lrc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            for x in bar.left..bar.right {
                let t = (x - bar.left) as f32 / span as f32;
                let c = match i {
                    0 => hsl_to_rgb(t * 360.0, 0.85, 0.55),
                    1 => hsl_to_rgb(hue, t, light),
                    _ => hsl_to_rgb(hue, sat, t),
                };
                SetDCBrushColor(mem, colorref(c));
                let col = RECT { left: x, top: bar.top, right: x + 1, bottom: bar.bottom };
                FillRect(mem, &col, dc_brush);
            }
            FrameRect(mem, &bar, black);

            // Thumb.
            let tx = bar.left + (value * span as f32) as i32;
            let thumb = RECT {
                left: tx - 3,
                top: bar.top - 4,
                right: tx + 4,
                bottom: bar.bottom + 4,
            };
            SetDCBrushColor(mem, COLORREF(0x00ffffff));
            FillRect(mem, &thumb, dc_brush);
            FrameRect(mem, &thumb, black);
        }

        // Preview swatch: the shallow→deep gradient this HSL produces.
        let (shallow, deep) = derive_rgb(st.shallow0, st.deep0, st.hsl);
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

/// Recolor the saved shallow/deep pair to the picked HSL: shallow takes it
/// directly, while deep keeps its hue/saturation/lightness offsets relative
/// to shallow, so the pair stays a coherent "water" gradient. At the saved
/// color's own HSL the inputs are returned untouched, so Reset → OK is an
/// exact no-op.
pub fn derive(shallow_hex: &str, deep_hex: &str, h: f32, s: f32, l: f32) -> (String, String) {
    let (sh, dp) = derive_rgb(parse_color(shallow_hex), parse_color(deep_hex), [h, s, l]);
    (hex(sh), hex(dp))
}

fn derive_rgb(shallow0: [f32; 3], deep0: [f32; 3], hsl: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let (h0, s0, l0) = rgb_to_hsl(shallow0);
    let (h_d, s_d, l_d) = rgb_to_hsl(deep0);
    let [h, s, l] = hsl;
    if hue_dist(h, h0) < 0.5 && (s - s0).abs() < 0.005 && (l - l0).abs() < 0.005 {
        return (shallow0, deep0);
    }
    (
        hsl_to_rgb(wrap_hue(h), s, l),
        hsl_to_rgb(
            wrap_hue(h_d + h - h0),
            (s_d + s - s0).clamp(0.0, 1.0),
            (l_d + l - l0).clamp(0.0, 1.0),
        ),
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
