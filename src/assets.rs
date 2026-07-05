//! Embedded sprite assets and atlas building.
//!
//! Each character resolves to a small RGBA atlas (premultiplied alpha) holding
//! its animation frames side by side, plus per-frame UV rects. Atlases are
//! built on demand when a character is selected — they are a few hundred KB at
//! most, so rebuild-on-switch is simpler than caching all of them.
//!
//! Credits:
//! - Duck: "Pixel Duck Anim SpriteSheet" by smolware (free), smolware.itch.io
//! - Capybara: itch.io-hosted pixel art (img.itch.zone 21409401, artist unidentified)
//! - Clownfish/coconut/boat: user-supplied stills; swim/flag/sail frames generated

use anyhow::{bail, Context, Result};

use crate::config::{Config, CustomSprite};

// Lazy float loop: hold head-up for a long stretch, then hold a small dip,
// so each pose lingers instead of the head flicking around.
static DUCK_FRAMES: [&[u8]; 9] = [
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_1.png"),
    include_bytes!("../assets/duck_float_2.png"),
    include_bytes!("../assets/duck_float_2.png"),
];
static CAPY_FRAMES: [&[u8]; 4] = [
    include_bytes!("../assets/capy_1.png"),
    include_bytes!("../assets/capy_2.png"),
    include_bytes!("../assets/capy_3.png"),
    include_bytes!("../assets/capy_4.png"),
];
static COCONUT_FRAMES: [&[u8]; 1] = [include_bytes!("../assets/coconut_1.png")];
static FISH_FRAMES: [&[u8]; 4] = [
    include_bytes!("../assets/fish_1.png"),
    include_bytes!("../assets/fish_2.png"),
    include_bytes!("../assets/fish_3.png"),
    include_bytes!("../assets/fish_4.png"),
];
static BOAT_FRAMES: [&[u8]; 4] = [
    include_bytes!("../assets/boat_1.png"),
    include_bytes!("../assets/boat_2.png"),
    include_bytes!("../assets/boat_3.png"),
    include_bytes!("../assets/boat_4.png"),
];
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facing {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaterMode {
    /// Rides on the surface with a reflection below (duck).
    Floater,
    /// Glides just under the surface, tinted by the water (fish).
    Swimmer,
}

/// A built-in character's frames: individual embedded PNGs, one per frame.
struct Frames(&'static [&'static [u8]]);

pub struct CharacterInfo {
    pub id: &'static str,
    pub name: &'static str,
    mode: WaterMode,
    facing: Facing,
    fps: f32,
    /// Where the water surface crosses the (trimmed) frame, 0 = top, 1 = bottom.
    /// Floaters sink below this line; pixels under it fade out underwater.
    waterline: f32,
    /// Idle rocking amplitude in radians (floaters sway side to side).
    rock: f32,
    frames: Frames,
}

/// Built-in roster shown in the tray menu, in display order.
pub const ROSTER: &[CharacterInfo] = &[
    CharacterInfo { id: "duck", name: "Duck", mode: WaterMode::Floater, facing: Facing::Right, fps: 1.0, waterline: 0.85, rock: 0.035, frames: Frames(&DUCK_FRAMES) },
    CharacterInfo { id: "capybara", name: "Capybara", mode: WaterMode::Floater, facing: Facing::Right, fps: 2.0, waterline: 0.65, rock: 0.03, frames: Frames(&CAPY_FRAMES) },
    CharacterInfo { id: "fish", name: "Clownfish", mode: WaterMode::Swimmer, facing: Facing::Left, fps: 6.0, waterline: 1.0, rock: 0.0, frames: Frames(&FISH_FRAMES) },
    CharacterInfo { id: "coconut", name: "Coconut", mode: WaterMode::Floater, facing: Facing::Right, fps: 1.0, waterline: 0.72, rock: 0.12, frames: Frames(&COCONUT_FRAMES) },
    CharacterInfo { id: "boat", name: "Sailboat", mode: WaterMode::Floater, facing: Facing::Right, fps: 5.0, waterline: 0.88, rock: 0.05, frames: Frames(&BOAT_FRAMES) },
];

/// A character's frames packed into one RGBA8 premultiplied atlas.
pub struct SpriteAtlas {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// UV rects (u0, v0, u1, v1) per animation frame.
    pub frames: Vec<[f32; 4]>,
    /// Frame size in atlas pixels (all frames share one trimmed size).
    pub frame_w: u32,
    pub frame_h: u32,
    pub fps: f32,
    pub mode: WaterMode,
    pub facing: Facing,
    /// Water-surface crossing as a fraction of frame height (0 top, 1 bottom).
    pub waterline: f32,
    /// Idle rocking amplitude in radians.
    pub rock: f32,
}

pub fn build_atlas(cfg: &Config) -> Result<SpriteAtlas> {
    if cfg.character == "custom" {
        let spec = cfg
            .custom_sprite
            .as_ref()
            .context("character is \"custom\" but [custom_sprite] is not configured")?;
        return build_custom(spec);
    }
    let info = ROSTER
        .iter()
        .find(|c| c.id == cfg.character)
        .with_context(|| format!("unknown character {:?}", cfg.character))?;

    let frames: Vec<Image> = info
        .frames
        .0
        .iter()
        .map(|bytes| decode_png(bytes))
        .collect::<Result<_>>()?;
    pack(frames, info.fps, info.mode, info.facing, info.waterline, info.rock)
}

fn build_custom(spec: &CustomSprite) -> Result<SpriteAtlas> {
    let bytes = std::fs::read(&spec.path)
        .with_context(|| format!("reading custom sprite {}", spec.path.display()))?;
    let sheet = decode_png(&bytes)?;
    if spec.frame_width == 0
        || spec.frame_height == 0
        || spec.frame_width > sheet.width
        || spec.frame_height > sheet.height
    {
        bail!(
            "custom sprite frame size {}x{} does not fit sheet {}x{}",
            spec.frame_width, spec.frame_height, sheet.width, sheet.height
        );
    }
    let count = (sheet.width / spec.frame_width).max(1);
    let frames = (0..count)
        .map(|i| sheet.crop(i * spec.frame_width, 0, spec.frame_width, spec.frame_height))
        .collect::<Result<Vec<_>>>()?;
    let mode = match spec.mode.as_str() {
        "swimmer" => WaterMode::Swimmer,
        _ => WaterMode::Floater,
    };
    let facing = match spec.facing.as_str() {
        "left" => Facing::Left,
        _ => Facing::Right,
    };
    // Custom floaters ride like the coconut: partly submerged, swaying
    // visibly — the most forgiving look for arbitrary user images.
    let (waterline, rock) = match mode {
        WaterMode::Floater => (0.72, 0.12),
        WaterMode::Swimmer => (1.0, 0.0),
    };
    pack(frames, spec.fps.clamp(0.5, 30.0), mode, facing, waterline, rock)
}

struct Image {
    width: u32,
    height: u32,
    /// RGBA8, straight alpha until `pack` premultiplies.
    data: Vec<u8>,
}

impl Image {
    fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Image> {
        if x + w > self.width || y + h > self.height {
            bail!("crop out of bounds");
        }
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for row in y..y + h {
            let start = ((row * self.width + x) * 4) as usize;
            data.extend_from_slice(&self.data[start..start + (w * 4) as usize]);
        }
        Ok(Image { width: w, height: h, data })
    }

    fn alpha_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (self.width, self.height, 0u32, 0u32);
        let mut any = false;
        for y in 0..self.height {
            for x in 0..self.width {
                if self.data[((y * self.width + x) * 4 + 3) as usize] > 8 {
                    any = true;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        any.then_some((x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }
}

fn decode_png(bytes: &[u8]) -> Result<Image> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().context("reading png header")?;
    let mut buf = vec![0u8; reader.output_buffer_size().context("png too large")?];
    let info = reader.next_frame(&mut buf).context("decoding png")?;
    let (width, height) = (info.width, info.height);
    buf.truncate(info.buffer_size());

    let data = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => bail!("unsupported png color type {other:?} (use RGBA)"),
    };
    Ok(Image { width, height, data })
}

/// Trim all frames to their shared tight alpha bounds, premultiply, and lay
/// them out horizontally with a 1px transparent gutter to keep bilinear
/// sampling from bleeding between frames.
fn pack(
    frames: Vec<Image>,
    fps: f32,
    mode: WaterMode,
    facing: Facing,
    waterline: f32,
    rock: f32,
) -> Result<SpriteAtlas> {
    if frames.is_empty() {
        bail!("sprite has no frames");
    }
    // Union of per-frame alpha bounds so every frame keeps the same anchor.
    let mut union: Option<(u32, u32, u32, u32)> = None;
    for f in &frames {
        if let Some((x, y, w, h)) = f.alpha_bounds() {
            union = Some(match union {
                None => (x, y, w, h),
                Some((ux, uy, uw, uh)) => {
                    let x0 = ux.min(x);
                    let y0 = uy.min(y);
                    let x1 = (ux + uw).max(x + w);
                    let y1 = (uy + uh).max(y + h);
                    (x0, y0, x1 - x0, y1 - y0)
                }
            });
        }
    }
    let (tx, ty, fw, fh) = union.context("sprite is fully transparent")?;

    let gutter = 1u32;
    let atlas_w = frames.len() as u32 * (fw + gutter) + gutter;
    let atlas_h = fh + 2 * gutter;
    let mut pixels = vec![0u8; (atlas_w * atlas_h * 4) as usize];
    let mut rects = Vec::with_capacity(frames.len());

    for (i, frame) in frames.iter().enumerate() {
        let dst_x = gutter + i as u32 * (fw + gutter);
        for row in 0..fh {
            for col in 0..fw {
                let src = (((ty + row) * frame.width + tx + col) * 4) as usize;
                let dst = (((gutter + row) * atlas_w + dst_x + col) * 4) as usize;
                let a = frame.data[src + 3] as u32;
                // Premultiply so the pipeline can use (ONE, INV_SRC_ALPHA).
                pixels[dst] = (frame.data[src] as u32 * a / 255) as u8;
                pixels[dst + 1] = (frame.data[src + 1] as u32 * a / 255) as u8;
                pixels[dst + 2] = (frame.data[src + 2] as u32 * a / 255) as u8;
                pixels[dst + 3] = a as u8;
            }
        }
        rects.push([
            dst_x as f32 / atlas_w as f32,
            gutter as f32 / atlas_h as f32,
            (dst_x + fw) as f32 / atlas_w as f32,
            (gutter + fh) as f32 / atlas_h as f32,
        ]);
    }

    Ok(SpriteAtlas {
        pixels,
        width: atlas_w,
        height: atlas_h,
        frames: rects,
        frame_w: fw,
        frame_h: fh,
        fps,
        mode,
        facing,
        waterline,
        rock,
    })
}
