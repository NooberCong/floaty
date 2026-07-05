//! User configuration: loaded from %APPDATA%\floaty\config.toml, hot-reloaded
//! when the file changes, and written back when tray-menu toggles change it.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Built-in ids: duck, capybara, fish, coconut, boat — or "custom", or
    /// "rotate" to switch between built-ins at random every `rotate_minutes`.
    pub character: String,
    /// How long each character stays on screen in rotate mode, in minutes.
    pub rotate_minutes: f32,
    /// Character size relative to taskbar height (0.2..=1.5).
    pub scale: f32,
    /// Swim speed multiplier (0.1..=4.0).
    pub speed: f32,
    /// Water body opacity (0.0..=1.0).
    pub water_opacity: f32,
    /// Water color at the surface, "#rrggbb".
    pub water_shallow: String,
    /// Water color at the bottom of the taskbar, "#rrggbb".
    pub water_deep: String,
    /// Strength of ambient waves and ripples (0.0..=2.0).
    pub wave_intensity: f32,
    /// Spawn ripples when the mouse moves across the taskbar.
    pub mouse_ripples: bool,
    /// Frame rate while water or character is in motion.
    pub active_fps: u32,
    /// Frame rate when the scene has settled to gentle idling.
    pub idle_fps: u32,
    /// Overlay secondary-monitor taskbars too.
    pub all_monitors: bool,
    /// Run at Windows startup (mirrored into the registry Run key).
    pub autostart: bool,
    /// Master switch; the tray menu Pause toggles this in memory only.
    pub enabled: bool,
    /// Optional user-supplied sprite sheet (horizontal strip of frames).
    pub custom_sprite: Option<CustomSprite>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomSprite {
    /// Path to a PNG containing a horizontal strip of equally sized frames.
    pub path: PathBuf,
    pub frame_width: u32,
    pub frame_height: u32,
    /// Animation speed in frames per second.
    #[serde(default = "default_custom_fps")]
    pub fps: f32,
    /// "floater" bobs on the surface (duck-like), "swimmer" glides submerged (fish-like).
    #[serde(default = "default_custom_mode")]
    pub mode: String,
    /// Direction the art faces in the sheet: "left" or "right".
    #[serde(default = "default_custom_facing")]
    pub facing: String,
}

fn default_custom_fps() -> f32 {
    4.0
}
fn default_custom_mode() -> String {
    "floater".into()
}
fn default_custom_facing() -> String {
    "right".into()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            character: "duck".into(),
            rotate_minutes: 5.0,
            scale: 0.8,
            speed: 1.0,
            water_opacity: 0.42,
            water_shallow: "#3f9de0".into(),
            water_deep: "#123f78".into(),
            wave_intensity: 1.0,
            mouse_ripples: true,
            active_fps: 60,
            idle_fps: 30,
            all_monitors: true,
            autostart: false,
            enabled: true,
            custom_sprite: None,
        }
    }
}

impl Config {
    fn clamped(mut self) -> Self {
        self.scale = self.scale.clamp(0.2, 1.5);
        self.rotate_minutes = self.rotate_minutes.clamp(0.2, 720.0);
        self.speed = self.speed.clamp(0.1, 4.0);
        self.water_opacity = self.water_opacity.clamp(0.0, 1.0);
        self.wave_intensity = self.wave_intensity.clamp(0.0, 2.0);
        self.active_fps = self.active_fps.clamp(10, 240);
        self.idle_fps = self.idle_fps.clamp(1, self.active_fps);
        self
    }
}

/// Parse "#rrggbb" into linear-ish RGB floats (kept in sRGB space; the shader
/// works in sRGB which is fine for this kind of stylized compositing).
pub fn parse_color(s: &str) -> [f32; 3] {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() == 6 {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return [
                ((v >> 16) & 0xff) as f32 / 255.0,
                ((v >> 8) & 0xff) as f32 / 255.0,
                (v & 0xff) as f32 / 255.0,
            ];
        }
    }
    log::warn!("invalid color {s:?}, using fallback");
    [0.25, 0.62, 0.88]
}

pub struct ConfigStore {
    pub current: Config,
    path: PathBuf,
    last_mtime: Option<SystemTime>,
}

impl ConfigStore {
    pub fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("floaty")
    }

    /// Load config, creating a commented default file on first run.
    pub fn load() -> Result<Self> {
        let dir = Self::config_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
        let path = dir.join("config.toml");

        let current = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => cfg.clamped(),
                Err(e) => {
                    log::error!("config parse error, using defaults: {e}");
                    Config::default()
                }
            },
            Err(_) => {
                let cfg = Config::default();
                if let Err(e) = write_default(&path, &cfg) {
                    log::error!("could not write default config: {e}");
                }
                cfg
            }
        };

        let last_mtime = mtime(&path);
        Ok(Self { current, path, last_mtime })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Returns true when the file changed on disk and parsed to a different config.
    pub fn poll_reload(&mut self) -> bool {
        let now = mtime(&self.path);
        if now == self.last_mtime {
            return false;
        }
        self.last_mtime = now;
        match std::fs::read_to_string(&self.path)
            .map_err(anyhow::Error::from)
            .and_then(|t| toml::from_str::<Config>(&t).map_err(Into::into))
        {
            Ok(cfg) => {
                let cfg = cfg.clamped();
                if cfg != self.current {
                    log::info!("config reloaded");
                    self.current = cfg;
                    return true;
                }
                false
            }
            Err(e) => {
                log::error!("config reload failed (keeping previous): {e}");
                false
            }
        }
    }

    /// Persist the in-memory config (used by tray toggles). Keeps hot-reload
    /// from bouncing by refreshing the stored mtime afterwards.
    pub fn save(&mut self) {
        match toml::to_string_pretty(&self.current) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&self.path, header() + &text) {
                    log::error!("config save failed: {e}");
                }
                self.last_mtime = mtime(&self.path);
            }
            Err(e) => log::error!("config serialize failed: {e}"),
        }
    }
}

fn mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn header() -> String {
    "# Floaty configuration — edits are applied live while Floaty runs.\n\
     # Characters: duck, capybara, fish, coconut, boat, custom,\n\
     # or \"rotate\" (random built-in every rotate_minutes)\n\n"
        .to_string()
}

fn write_default(path: &PathBuf, cfg: &Config) -> Result<()> {
    let text = toml::to_string_pretty(cfg)?;
    std::fs::write(path, header() + &text)?;
    Ok(())
}
