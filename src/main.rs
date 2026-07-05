//! Floaty — turns the Windows taskbar into a 2D pool with ripples and a
//! floating character. Native Win32 + Direct3D 11 + DirectComposition.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod autostart;
mod character;
mod colorpicker;
mod config;
mod gfx;
mod import;
mod overlay;
mod sim;
mod taskbar;
mod tray;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, ShellExecuteW, QUNS_ACCEPTS_NOTIFICATIONS, QUNS_QUIET_TIME};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::assets::{SpriteAtlas, WaterMode};
use crate::character::Character;
use crate::colorpicker::{WM_WATER_CANCEL, WM_WATER_COMMIT, WM_WATER_PREVIEW};
use crate::config::{parse_color, ConfigStore};
use crate::gfx::{Gfx, Params, SpriteDraw, SpriteQuad, SpriteTexture, Surface};
use crate::sim::RippleSim;
use crate::taskbar::TaskbarInfo;
use crate::tray::{Tray, TrayCommand, WM_TRAYICON};

/// Ambient wave time coefficients all have ≤2 decimals, so wrapping shader
/// time at 2000π keeps every sin() continuous while preserving f32 precision.
const TIME_WRAP: f64 = 2000.0 * std::f64::consts::PI;

fn main() {
    init_logging();
    if already_running() {
        unsafe {
            MessageBoxW(
                None,
                w!("Floaty is already running — look for the duck in the system tray."),
                w!("Floaty"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        return;
    }
    if let Err(e) = run() {
        log::error!("fatal: {e:#}");
        let msg: Vec<u16> = format!("Floaty failed to start:\n\n{e:#}\0").encode_utf16().collect();
        unsafe {
            MessageBoxW(None, PCWSTR(msg.as_ptr()), w!("Floaty"), MB_OK | MB_ICONERROR);
        }
        std::process::exit(1);
    }
}

fn init_logging() {
    let dir = ConfigStore::config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let level = if cfg!(debug_assertions) {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    if let Ok(file) = std::fs::File::create(dir.join("floaty.log")) {
        let _ = simplelog::WriteLogger::init(level, simplelog::Config::default(), file);
    }
}

fn already_running() -> bool {
    unsafe {
        let _ = CreateMutexW(None, false, w!("Local\\FloatySingleInstance"));
        windows::core::Error::from_thread().code()
            == windows::Win32::Foundation::ERROR_ALREADY_EXISTS.to_hresult()
    }
}

fn run() -> Result<()> {
    let store = ConfigStore::load()?;
    autostart::sync(store.current.autostart);

    overlay::register_class()?;
    let msg_hwnd = create_message_window()?;
    let tray = Tray::new(msg_hwnd)?;

    let gfx = Gfx::new()?;
    let mut rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15)
        | 1;
    let rotate_id = random_roster_id(&mut rng, "").to_string();
    let atlas = assets::build_atlas(&effective_config(&store.current, &rotate_id)).or_else(|e| {
        log::error!("configured character failed ({e:#}); falling back to duck");
        let mut cfg = store.current.clone();
        cfg.character = "duck".into();
        assets::build_atlas(&cfg)
    })?;
    let sprite_tex = gfx.upload_atlas(&atlas)?;

    let mut app = Box::new(App {
        store,
        gfx,
        atlas,
        sprite_tex,
        overlays: Vec::new(),
        tray,
        msg_hwnd,
        water_preview: None,
        picker: None,
        taskbar_created_msg: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
        started: Instant::now(),
        last_tick: Instant::now(),
        next_frame: Instant::now(),
        sim_accum: 0.0,
        last_slow_poll: Instant::now() - Duration::from_secs(10),
        suspended: false,
        need_refresh: true,
        last_cursor: POINT::default(),
        quitting: false,
        rng,
        rotate_id,
        next_rotate: Instant::now(),
    });

    unsafe {
        SetWindowLongPtrW(msg_hwnd, GWLP_USERDATA, &mut *app as *mut App as isize);
    }

    log::info!("floaty started");
    // Dev hook: open the water color picker immediately (tray menu shortcut
    // for automated testing — harmless if set by accident).
    if std::env::var_os("FLOATY_OPEN_PICKER").is_some() {
        app.on_command(TrayCommand::WaterColor);
    }
    // Dev hook: import an image without the file dialog (automated testing).
    if let Some(path) = std::env::var_os("FLOATY_IMPORT") {
        match import::import_file(std::path::Path::new(&path)) {
            Ok(spec) => app.apply_import(spec),
            Err(e) => log::error!("FLOATY_IMPORT failed: {e:#}"),
        }
    }
    let mut msg = MSG::default();
    loop {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    app.quitting = true;
                } else {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        if app.quitting {
            break;
        }
        app.tick();
        let wait = app
            .next_frame
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        unsafe {
            MsgWaitForMultipleObjects(None, false, wait.as_millis() as u32, QS_ALLINPUT);
        }
    }

    // Explicit teardown before the Box drops: hide our windows immediately.
    for ov in app.overlays.drain(..) {
        overlay::destroy(ov.hwnd);
    }
    unsafe { SetWindowLongPtrW(msg_hwnd, GWLP_USERDATA, 0) };
    log::info!("floaty exiting");
    Ok(())
}

struct OverlayState {
    info: TaskbarInfo,
    hwnd: HWND,
    surface: Surface,
    sim: RippleSim,
    character: Character,
}

struct App {
    store: ConfigStore,
    gfx: Gfx,
    atlas: SpriteAtlas,
    sprite_tex: SpriteTexture,
    overlays: Vec<OverlayState>,
    tray: Tray,
    msg_hwnd: HWND,
    /// Water colors being previewed by the color picker; overrides the config
    /// colors on screen until committed (saved) or cancelled (discarded).
    water_preview: Option<(String, String)>,
    /// The color picker window, while one is open.
    picker: Option<HWND>,
    taskbar_created_msg: u32,
    started: Instant,
    last_tick: Instant,
    next_frame: Instant,
    sim_accum: f32,
    last_slow_poll: Instant,
    suspended: bool,
    need_refresh: bool,
    last_cursor: POINT,
    quitting: bool,
    /// Rotate-mode state: xorshift seed, the built-in currently shown, and
    /// when to switch to the next random one.
    rng: u64,
    rotate_id: String,
    next_rotate: Instant,
}

/// Resolve "rotate" to the currently chosen built-in; other ids pass through.
fn effective_config(cfg: &config::Config, rotate_id: &str) -> config::Config {
    if cfg.character == "rotate" {
        let mut c = cfg.clone();
        c.character = rotate_id.to_string();
        c
    } else {
        cfg.clone()
    }
}

/// Pick a random built-in id, avoiding `except` so rotation always changes.
fn random_roster_id(rng: &mut u64, except: &str) -> &'static str {
    loop {
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        let pick = assets::ROSTER[(*rng >> 33) as usize % assets::ROSTER.len()].id;
        if pick != except || assets::ROSTER.len() == 1 {
            return pick;
        }
    }
}

impl App {
    fn tick(&mut self) {
        let now = Instant::now();

        // Slow housekeeping: taskbar geometry, config hot-reload, fullscreen.
        if now.duration_since(self.last_slow_poll) >= Duration::from_secs(1) || self.need_refresh {
            self.last_slow_poll = now;
            self.slow_poll();
        }

        if now < self.next_frame {
            return;
        }
        let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
        self.last_tick = now;

        let enabled = self.store.current.enabled && !self.suspended;
        if !enabled || self.overlays.is_empty() {
            self.next_frame = now + Duration::from_millis(300);
            return;
        }

        let cursor = cursor_pos();
        let cursor_moved = ((cursor.x - self.last_cursor.x).pow(2)
            + (cursor.y - self.last_cursor.y).pow(2)) as f32;

        // Fixed-step the wave equation at 60 Hz so ripple propagation speed is
        // identical at any render rate.
        self.sim_accum = (self.sim_accum + dt).min(0.25);
        let mut steps = 0;
        while self.sim_accum >= 1.0 / 60.0 {
            self.sim_accum -= 1.0 / 60.0;
            steps += 1;
        }

        let cfg = self.store.current.clone();
        // While the color picker is open its previewed colors win over config.
        let (col_shallow, col_deep) = match &self.water_preview {
            Some((s, d)) => (parse_color(s), parse_color(d)),
            None => (parse_color(&cfg.water_shallow), parse_color(&cfg.water_deep)),
        };
        let time = (self.started.elapsed().as_secs_f64() % TIME_WRAP) as f32;
        let mut any_active = false;

        for ov in &mut self.overlays {
            if !ov.info.usable() {
                continue;
            }
            let bar_w = ov.info.width() as f32;
            let bar_h = ov.info.height() as f32;

            // Mouse ripples: stir the water where the cursor crosses the bar.
            if cfg.mouse_ripples && cursor_moved > 4.0 {
                let rx = (cursor.x - ov.info.rect.left) as f32;
                let ry = (cursor.y - ov.info.rect.top) as f32;
                if rx >= 0.0 && rx < bar_w && ry >= 0.0 && ry < bar_h {
                    let strength = (cursor_moved.sqrt() * 0.012).clamp(0.05, 0.45);
                    ov.sim.splash(rx, ry, 4.5, strength * cfg.wave_intensity.max(0.1));
                }
            }

            for _ in 0..steps {
                ov.sim.step();
            }

            // Character: size from bar height, pose from water + wander AI.
            let sprite_h = (bar_h * cfg.scale * 0.9).clamp(12.0, bar_h * 1.4);
            let sprite_w = sprite_h * self.atlas.frame_w as f32 / self.atlas.frame_h as f32;
            // Floaters get a little headroom below the top of the bar.
            let float_top = bar_h * 0.10;
            // Where the character meets the water: a floater's waterline is a
            // per-character fraction of its sprite, a swimmer stirs mid-water.
            let splash_y = match self.atlas.mode {
                WaterMode::Floater => float_top + sprite_h * (0.02 + self.atlas.waterline),
                WaterMode::Swimmer => bar_h * 0.52,
            };
            let pose = ov.character.update(
                dt,
                bar_w,
                sprite_w,
                splash_y,
                cfg.speed,
                &self.atlas,
                &mut ov.sim,
            );

            let params = Params {
                viewport: [bar_w, bar_h],
                time,
                wave_intensity: cfg.wave_intensity,
                shallow: col_shallow,
                opacity: cfg.water_opacity,
                deep: col_deep,
                ..Default::default()
            };

            let uv = self.atlas.frames[pose.frame];
            let cy = match self.atlas.mode {
                // Floaters ride the surface, bodies dipping just below it.
                WaterMode::Floater => float_top + sprite_h * 0.52 + pose.y_offset,
                // Swimmers glide mid-water.
                WaterMode::Swimmer => bar_h * 0.52 + pose.y_offset * 0.6,
            };
            // Water surface at the character (moves with the bob); pixels
            // below it fade out underwater in the shader.
            let waterline = cy + sprite_h * (self.atlas.waterline - 0.5);
            let body_clip = match self.atlas.mode {
                WaterMode::Floater => waterline,
                WaterMode::Swimmer => 0.0,
            };
            let body = SpriteQuad {
                rect: [pose.x, cy, sprite_w, sprite_h],
                uv,
                misc: [pose.tilt, pose.facing, body_clip, 0.0],
            };
            let reflection = (self.atlas.mode == WaterMode::Floater).then(|| SpriteQuad {
                rect: [pose.x, 2.0 * waterline - cy, sprite_w, sprite_h],
                uv: [uv[0], uv[3], uv[2], uv[1]], // flip v
                misc: [-pose.tilt, pose.facing, 0.0, 1.0],
            });

            let draw = SpriteDraw { body, reflection };
            if let Err(e) = ov.surface.upload_heightfield(&self.gfx, ov.sim.field()) {
                log::error!("heightfield upload failed: {e:#}");
            }
            match ov.surface.render(&self.gfx, &params, Some((&self.sprite_tex, draw))) {
                Ok(()) => {
                    if let Err(e) = ov.surface.present() {
                        log::warn!("present failed: {e:#}");
                    }
                }
                Err(e) => log::error!("render failed: {e:#}"),
            }

            any_active |= pose.moving || !ov.sim.is_calm();
        }
        self.last_cursor = cursor;

        let fps = if any_active { cfg.active_fps } else { cfg.idle_fps };
        self.next_frame = now + Duration::from_secs_f64(1.0 / fps as f64);
    }

    fn slow_poll(&mut self) {
        self.need_refresh = false;

        if self.store.poll_reload() {
            self.apply_config_change();
        }

        // Rotate mode: swap in a different random built-in on schedule.
        if self.store.current.character == "rotate" && Instant::now() >= self.next_rotate {
            self.rotate_id = random_roster_id(&mut self.rng, &self.rotate_id).to_string();
            self.next_rotate = Instant::now()
                + Duration::from_secs_f32(self.store.current.rotate_minutes * 60.0);
            log::info!("rotate mode: switching to {}", self.rotate_id);
            self.apply_config_change();
        }

        // Suspend while a fullscreen app / presentation runs on any monitor.
        let state = unsafe { SHQueryUserNotificationState() };
        let suspended =
            matches!(state, Ok(s) if s != QUNS_ACCEPTS_NOTIFICATIONS && s != QUNS_QUIET_TIME);
        if suspended != self.suspended {
            self.suspended = suspended;
            log::info!("fullscreen suspend: {suspended}");
        }

        self.sync_overlays();
    }

    /// Reconcile overlay windows with the taskbars that exist right now.
    fn sync_overlays(&mut self) {
        let enabled = self.store.current.enabled && !self.suspended;
        let bars = if enabled {
            taskbar::enumerate(self.store.current.all_monitors)
        } else {
            Vec::new()
        };

        // Drop overlays whose taskbar vanished (or everything when disabled).
        self.overlays.retain(|ov| {
            let keep = bars.iter().any(|b| b.hwnd == ov.info.hwnd);
            if !keep {
                overlay::destroy(ov.hwnd);
            }
            keep
        });

        for bar in bars {
            if !bar.usable() {
                // Auto-hidden or vertical: hide any existing overlay for it.
                if let Some(ov) = self.overlays.iter_mut().find(|o| o.info.hwnd == bar.hwnd) {
                    unsafe {
                        let _ = ShowWindow(ov.hwnd, SW_HIDE);
                    }
                    ov.info = bar;
                }
                continue;
            }
            match self.overlays.iter_mut().find(|o| o.info.hwnd == bar.hwnd) {
                Some(ov) => {
                    let resized =
                        ov.info.width() != bar.width() || ov.info.height() != bar.height();
                    // Reassert position and z-order every poll: the shell
                    // sometimes hoists the taskbar above topmost overlays.
                    if let Err(e) = overlay::place(
                        ov.hwnd,
                        bar.rect.left,
                        bar.rect.top,
                        bar.width(),
                        bar.height(),
                    ) {
                        log::warn!("overlay reposition failed: {e:#}");
                    }
                    if resized {
                        let (w, h) = (bar.width() as u32, bar.height() as u32);
                        let sim = RippleSim::new(w, h);
                        if let Err(e) = ov.surface.resize(&self.gfx, w, h, sim.width, sim.height) {
                            log::error!("surface resize failed: {e:#}");
                            continue;
                        }
                        ov.sim = sim;
                        ov.character.clamp_to(w as f32);
                    }
                    ov.info = bar;
                }
                None => match self.create_overlay(&bar) {
                    Ok(ov) => self.overlays.push(ov),
                    Err(e) => log::error!("overlay create failed: {e:#}"),
                },
            }
        }
    }

    fn create_overlay(&self, bar: &TaskbarInfo) -> Result<OverlayState> {
        let (w, h) = (bar.width() as u32, bar.height() as u32);
        let hwnd = overlay::create(bar.rect.left, bar.rect.top, bar.width(), bar.height())?;
        let sim = RippleSim::new(w, h);
        let surface = Surface::new(&self.gfx, hwnd, w, h, sim.width, sim.height)
            .inspect_err(|_| overlay::destroy(hwnd))?;
        let seed = bar.hwnd.0 as u64 ^ self.started.elapsed().as_nanos() as u64;
        log::info!(
            "overlay created for {} taskbar {}x{}",
            if bar.primary { "primary" } else { "secondary" },
            w,
            h
        );
        Ok(OverlayState {
            info: bar.clone(),
            hwnd,
            surface,
            sim,
            character: Character::new(w as f32, seed),
        })
    }

    fn apply_config_change(&mut self) {
        autostart::sync(self.store.current.autostart);
        match assets::build_atlas(&effective_config(&self.store.current, &self.rotate_id)) {
            Ok(atlas) => match self.gfx.upload_atlas(&atlas) {
                Ok(tex) => {
                    self.atlas = atlas;
                    self.sprite_tex = tex;
                }
                Err(e) => log::error!("atlas upload failed: {e:#}"),
            },
            Err(e) => log::error!("character rebuild failed, keeping current: {e:#}"),
        }
        self.sync_overlays();
    }

    fn on_tray(&mut self, event: u32) {
        match event {
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                let cfg = &self.store.current;
                let cmd = self.tray.show_menu(
                    !cfg.enabled,
                    cfg.mouse_ripples,
                    cfg.autostart,
                    &cfg.character,
                    cfg.custom_sprite.is_some(),
                );
                if let Some(cmd) = cmd {
                    self.on_command(cmd);
                }
            }
            WM_LBUTTONDBLCLK => self.on_command(TrayCommand::TogglePause),
            _ => {}
        }
    }

    /// Switch to a freshly imported custom image and persist it.
    fn apply_import(&mut self, spec: config::CustomSprite) {
        self.store.current.custom_sprite = Some(spec);
        self.store.current.character = "custom".into();
        self.store.save();
        self.apply_config_change();
    }

    fn on_command(&mut self, cmd: TrayCommand) {
        match cmd {
            TrayCommand::TogglePause => {
                self.store.current.enabled = !self.store.current.enabled;
                self.sync_overlays();
            }
            TrayCommand::ToggleMouseRipples => {
                self.store.current.mouse_ripples = !self.store.current.mouse_ripples;
                self.store.save();
            }
            TrayCommand::ToggleAutostart => {
                self.store.current.autostart = !self.store.current.autostart;
                autostart::sync(self.store.current.autostart);
                self.store.save();
            }
            TrayCommand::WaterColor => {
                // One picker at a time: refocus the open one instead.
                if let Some(hwnd) = self.picker {
                    unsafe {
                        let _ = SetForegroundWindow(hwnd);
                    }
                } else {
                    let cfg = &self.store.current;
                    match colorpicker::open(self.msg_hwnd, &cfg.water_shallow, &cfg.water_deep) {
                        Ok(hwnd) => self.picker = Some(hwnd),
                        Err(e) => log::error!("color picker failed: {e:#}"),
                    }
                }
            }
            TrayCommand::SelectCharacter(id) => {
                self.store.current.character = id;
                self.store.save();
                self.apply_config_change();
            }
            TrayCommand::ImportImage => match import::pick_and_import(self.msg_hwnd) {
                Ok(Some(spec)) => self.apply_import(spec),
                Ok(None) => {} // dialog cancelled
                Err(e) => {
                    log::error!("image import failed: {e:#}");
                    let msg: Vec<u16> = format!("Couldn't import that image:\n\n{e:#}\0")
                        .encode_utf16()
                        .collect();
                    unsafe {
                        MessageBoxW(None, PCWSTR(msg.as_ptr()), w!("Floaty"), MB_OK | MB_ICONERROR);
                    }
                }
            },
            TrayCommand::OpenConfig => {
                let path: Vec<u16> = format!("{}\0", self.store.path().display())
                    .encode_utf16()
                    .collect();
                unsafe {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        PCWSTR(path.as_ptr()),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
            }
            TrayCommand::About => unsafe {
                MessageBoxW(
                    None,
                    w!("Floaty — a tiny pool on your taskbar.\n\
                        by NooberCong\n\n\
                        Sprites:\n\
                        \u{2022} Duck by smolware (smolware.itch.io)\n\
                        \u{2022} Capybara from itch.io (artist unidentified)\n\
                        \u{2022} Clownfish, coconut, boat: user-supplied art\n\n\
                        Right-click the tray icon for settings."),
                    w!("About Floaty"),
                    MB_OK | MB_ICONINFORMATION,
                );
            },
            TrayCommand::Exit => unsafe { PostQuitMessage(0) },
        }
    }
}

fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}

// ------------------------------------------------------- message window ----

fn create_message_window() -> Result<HWND> {
    let hinstance = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW")?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(msg_proc),
        hInstance: hinstance.into(),
        lpszClassName: w!("FloatyMsg"),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        anyhow::bail!("RegisterClassW(FloatyMsg) failed");
    }
    // A hidden real window (not message-only): the tray menu needs a window
    // that can take foreground so it dismisses correctly.
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("FloatyMsg"),
            w!("Floaty"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
    }
    .context("creating message window")
}

extern "system" fn msg_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let app = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App };
    if !app.is_null() {
        let app = unsafe { &mut *app };
        match msg {
            WM_TRAYICON => {
                app.on_tray(lparam.0 as u32);
                return LRESULT(0);
            }
            WM_WATER_PREVIEW => {
                let hue = lparam.0 as f32 / 10.0;
                // wparam 1 = "Reset to default" was clicked: recolor the
                // stock palette instead of the saved one.
                let base = if wparam.0 == 1 {
                    config::Config::default()
                } else {
                    app.store.current.clone()
                };
                app.water_preview =
                    Some(colorpicker::derive(&base.water_shallow, &base.water_deep, hue));
                return LRESULT(0);
            }
            WM_WATER_COMMIT => {
                if let Some((shallow, deep)) = app.water_preview.take() {
                    app.store.current.water_shallow = shallow;
                    app.store.current.water_deep = deep;
                    app.store.save();
                    log::info!(
                        "water color set to {} / {}",
                        app.store.current.water_shallow,
                        app.store.current.water_deep
                    );
                }
                app.picker = None;
                return LRESULT(0);
            }
            WM_WATER_CANCEL => {
                app.water_preview = None;
                app.picker = None;
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE | WM_DPICHANGED => {
                app.need_refresh = true;
            }
            m if m == app.taskbar_created_msg && m != 0 => {
                // Explorer restarted: tray icon and overlays are gone.
                log::info!("explorer restarted; re-adding tray icon and overlays");
                if let Ok(tray) = Tray::new(hwnd) {
                    app.tray = tray;
                }
                for ov in app.overlays.drain(..) {
                    overlay::destroy(ov.hwnd);
                }
                app.need_refresh = true;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
