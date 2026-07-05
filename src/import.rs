//! "Import image…" — pick any image Windows can decode (PNG, JPEG, GIF, BMP,
//! WebP, TIFF, …) and turn it into the custom character.
//!
//! The image is decoded through WIC, downscaled once to sprite size, and saved
//! as a normalized RGBA PNG in the config dir. Importing copies pixels instead
//! of referencing the original file, so the character keeps working if the
//! source image later moves or is deleted.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{GENERIC_READ, HWND};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICBitmapSource, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

use crate::config::{ConfigStore, CustomSprite};

/// Imports are downscaled (never upscaled) to fit this box. Taskbars top out
/// around 100 px tall even at high DPI, so 128 px keeps the sprite crisp at
/// any scale without wasting atlas texture on photo-sized bitmaps.
const MAX_W: u32 = 256;
const MAX_H: u32 = 128;

/// Show the file dialog and import the chosen image. `Ok(None)` = cancelled.
pub fn pick_and_import(owner: HWND) -> Result<Option<CustomSprite>> {
    match pick_file(owner)? {
        Some(path) => import_file(&path).map(Some),
        None => Ok(None),
    }
}

/// Convert `src` into the custom character sprite and return its config entry.
pub fn import_file(src: &Path) -> Result<CustomSprite> {
    let (pixels, w, h) = decode_and_fit(src)?;
    if !pixels.chunks_exact(4).any(|p| p[3] > 8) {
        bail!("the image is fully transparent");
    }
    let dst = ConfigStore::config_dir().join("custom_sprite.png");
    write_png(&dst, &pixels, w, h)?;
    log::info!("imported {} as {w}x{h} custom sprite", src.display());
    Ok(CustomSprite {
        path: dst,
        frame_width: w,
        frame_height: h,
        fps: 1.0,
        mode: "floater".into(),
        facing: "right".into(),
    })
}

fn pick_file(owner: HWND) -> Result<Option<PathBuf>> {
    // NUL-separated name/pattern pairs, double-NUL terminated. WIC decodes
    // more formats than these, so keep an "All files" escape hatch.
    let filter: Vec<u16> = "Images\0*.png;*.jpg;*.jpeg;*.gif;*.bmp;*.webp;*.tif;*.tiff;*.ico\0\
                            All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Choose a character image\0".encode_utf16().collect();
    let mut file = [0u16; 1024];
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // Returns FALSE for both cancel and error; treat either as "no pick".
    if !unsafe { GetOpenFileNameW(&mut ofn) }.as_bool() {
        return Ok(None);
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(0);
    Ok(Some(PathBuf::from(String::from_utf16_lossy(&file[..len]))))
}

/// Decode any WIC-supported format to RGBA8, downscaled to fit MAX_W×MAX_H.
fn decode_and_fit(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    unsafe {
        // Idempotent per thread; an "already initialized" HRESULT is fine.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .context("creating WIC factory")?;

        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .context("Windows could not decode this image")?;
        // Frame 0 only: animated GIFs import as a still; motion comes from
        // Floaty's own bob/rock animation.
        let frame = decoder.GetFrame(0).context("reading image frame")?;

        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).context("reading image size")?;
        if w == 0 || h == 0 {
            bail!("the image is empty");
        }

        let scale = (MAX_W as f64 / w as f64).min(MAX_H as f64 / h as f64);
        let source: IWICBitmapSource = if scale < 1.0 {
            let (sw, sh) = (
                ((w as f64 * scale).round() as u32).max(1),
                ((h as f64 * scale).round() as u32).max(1),
            );
            let scaler = factory.CreateBitmapScaler().context("creating scaler")?;
            scaler
                .Initialize(&frame, sw, sh, WICBitmapInterpolationModeFant)
                .context("downscaling image")?;
            (w, h) = (sw, sh);
            scaler.cast()?
        } else {
            frame.cast()?
        };

        let converter = factory.CreateFormatConverter().context("creating converter")?;
        converter
            .Initialize(
                &source,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .context("converting image to RGBA")?;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        converter
            .CopyPixels(std::ptr::null(), w * 4, &mut pixels)
            .context("reading image pixels")?;
        Ok((pixels, w, h))
    }
}

fn write_png(path: &Path, pixels: &[u8], w: u32, h: u32) -> Result<()> {
    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("writing png header")?;
    writer.write_image_data(pixels).context("writing png data")?;
    Ok(())
}
