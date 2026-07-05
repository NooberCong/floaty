//! Direct3D 11 + DirectComposition rendering.
//!
//! One shared D3D device renders every overlay; each overlay owns a
//! premultiplied-alpha composition swapchain bound to its window through a
//! DirectComposition visual, which is the zero-copy path for per-pixel
//! transparent GPU content (no GDI, no redirection surface).

use anyhow::{bail, Context, Result};
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use crate::assets::SpriteAtlas;

const SHADER_SRC: &str = include_str!("shaders.hlsl");

/// Matches `cbuffer Params` in shaders.hlsl (16-byte aligned rows).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Params {
    pub viewport: [f32; 2],
    pub time: f32,
    pub wave_intensity: f32,
    pub shallow: [f32; 3],
    pub opacity: f32,
    pub deep: [f32; 3],
    pub _pad0: f32,
    pub sprite_rect: [f32; 4],
    pub sprite_uv: [f32; 4],
    pub sprite_misc: [f32; 4],
}

pub struct Gfx {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    dcomp: IDCompositionDevice,
    dxgi_factory: IDXGIFactory2,
    water_vs: ID3D11VertexShader,
    water_ps: ID3D11PixelShader,
    sprite_vs: ID3D11VertexShader,
    sprite_ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    cbuf: ID3D11Buffer,
    raster: ID3D11RasterizerState,
}

impl Gfx {
    pub fn new() -> Result<Self> {
        let (device, context) = create_device()?;

        let dxgi_device: IDXGIDevice = device.cast().context("querying IDXGIDevice")?;
        let adapter = unsafe { dxgi_device.GetAdapter() }.context("getting adapter")?;
        let dxgi_factory: IDXGIFactory2 =
            unsafe { adapter.GetParent() }.context("getting DXGI factory")?;
        let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_device) }
            .context("creating DirectComposition device")?;

        let water_vs_blob = compile("water_vs", "vs_5_0")?;
        let water_ps_blob = compile("water_ps", "ps_5_0")?;
        let sprite_vs_blob = compile("sprite_vs", "vs_5_0")?;
        let sprite_ps_blob = compile("sprite_ps", "ps_5_0")?;

        let mut water_vs = None;
        let mut water_ps = None;
        let mut sprite_vs = None;
        let mut sprite_ps = None;
        unsafe {
            device.CreateVertexShader(blob_bytes(&water_vs_blob), None, Some(&mut water_vs))?;
            device.CreatePixelShader(blob_bytes(&water_ps_blob), None, Some(&mut water_ps))?;
            device.CreateVertexShader(blob_bytes(&sprite_vs_blob), None, Some(&mut sprite_vs))?;
            device.CreatePixelShader(blob_bytes(&sprite_ps_blob), None, Some(&mut sprite_ps))?;
        }

        // Premultiplied alpha over the (transparent) target.
        let blend_desc = D3D11_BLEND_DESC {
            RenderTarget: [D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            }; 8],
            ..Default::default()
        };
        let mut blend = None;
        unsafe { device.CreateBlendState(&blend_desc, Some(&mut blend))? };

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler))? };

        let cb_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<Params>().next_multiple_of(16) as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cbuf = None;
        unsafe { device.CreateBuffer(&cb_desc, None, Some(&mut cbuf))? };

        // No culling: sprites render mirrored (negative x scale) when facing
        // the other way, which reverses winding and would be culled otherwise.
        let raster_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ..Default::default()
        };
        let mut raster = None;
        unsafe { device.CreateRasterizerState(&raster_desc, Some(&mut raster))? };

        Ok(Self {
            device,
            context,
            dcomp,
            dxgi_factory,
            water_vs: water_vs.unwrap(),
            water_ps: water_ps.unwrap(),
            sprite_vs: sprite_vs.unwrap(),
            sprite_ps: sprite_ps.unwrap(),
            blend: blend.unwrap(),
            sampler: sampler.unwrap(),
            cbuf: cbuf.unwrap(),
            raster: raster.unwrap(),
        })
    }

    pub fn upload_atlas(&self, atlas: &SpriteAtlas) -> Result<SpriteTexture> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: atlas.width,
            Height: atlas.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: atlas.pixels.as_ptr() as *const _,
            SysMemPitch: atlas.width * 4,
            ..Default::default()
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))? };
        let tex = tex.unwrap();
        let mut srv = None;
        unsafe { self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))? };
        Ok(SpriteTexture { _tex: tex, srv: srv.unwrap() })
    }

    fn write_params(&self, p: &Params) -> Result<()> {
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping(p, mapped.pData as *mut Params, 1);
            self.context.Unmap(&self.cbuf, 0);
        }
        Ok(())
    }
}

pub struct SpriteTexture {
    _tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
}

/// Per-overlay swapchain + heightfield texture.
pub struct Surface {
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    pub width: u32,
    pub height: u32,
    height_tex: ID3D11Texture2D,
    height_srv: ID3D11ShaderResourceView,
    grid_w: u32,
    grid_h: u32,
}

impl Surface {
    pub fn new(gfx: &Gfx, hwnd: HWND, width: u32, height: u32, grid_w: u32, grid_h: u32) -> Result<Self> {
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Scaling: DXGI_SCALING_STRETCH,
            ..Default::default()
        };
        let swapchain = unsafe {
            gfx.dxgi_factory
                .CreateSwapChainForComposition(&gfx.device, &desc, None)
        }
        .context("creating composition swapchain")?;

        let target = unsafe { gfx.dcomp.CreateTargetForHwnd(hwnd, true) }
            .context("creating DComp target")?;
        let visual = unsafe { gfx.dcomp.CreateVisual() }.context("creating DComp visual")?;
        unsafe {
            visual.SetContent(&swapchain).context("binding swapchain to visual")?;
            target.SetRoot(&visual).context("setting visual root")?;
            gfx.dcomp.Commit().context("dcomp commit")?;
        }

        let (height_tex, height_srv) = create_height_texture(&gfx.device, grid_w, grid_h)?;
        let rtv = create_rtv(&gfx.device, &swapchain)?;

        Ok(Self {
            swapchain,
            rtv: Some(rtv),
            _target: target,
            _visual: visual,
            width,
            height,
            height_tex,
            height_srv,
            grid_w,
            grid_h,
        })
    }

    pub fn resize(&mut self, gfx: &Gfx, width: u32, height: u32, grid_w: u32, grid_h: u32) -> Result<()> {
        self.rtv = None;
        unsafe {
            self.swapchain
                .ResizeBuffers(2, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))
        }
        .context("resizing swapchain")?;
        self.rtv = Some(create_rtv(&gfx.device, &self.swapchain)?);
        self.width = width;
        self.height = height;
        if (grid_w, grid_h) != (self.grid_w, self.grid_h) {
            let (tex, srv) = create_height_texture(&gfx.device, grid_w, grid_h)?;
            self.height_tex = tex;
            self.height_srv = srv;
            self.grid_w = grid_w;
            self.grid_h = grid_h;
        }
        Ok(())
    }

    pub fn upload_heightfield(&self, gfx: &Gfx, field: &[f32]) -> Result<()> {
        debug_assert_eq!(field.len(), (self.grid_w * self.grid_h) as usize);
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            gfx.context
                .Map(&self.height_tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            let row_bytes = self.grid_w as usize * 4;
            for y in 0..self.grid_h as usize {
                let src = &field[y * self.grid_w as usize..][..self.grid_w as usize];
                let dst = (mapped.pData as *mut u8).add(y * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(src.as_ptr() as *const u8, dst, row_bytes);
            }
            gfx.context.Unmap(&self.height_tex, 0);
        }
        Ok(())
    }

    /// Draw one frame: water, then (optionally) reflection + character.
    pub fn render(
        &self,
        gfx: &Gfx,
        base: &Params,
        sprite: Option<(&SpriteTexture, SpriteDraw)>,
    ) -> Result<()> {
        let Some(rtv) = &self.rtv else { return Ok(()) };
        unsafe {
            let ctx = &gfx.context;
            ctx.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 0.0]);
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.OMSetBlendState(&gfx.blend, None, u32::MAX);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: self.width as f32,
                Height: self.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            ctx.IASetPrimitiveTopology(
                windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            );
            ctx.RSSetState(&gfx.raster);
            ctx.VSSetConstantBuffers(0, Some(&[Some(gfx.cbuf.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(gfx.cbuf.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(gfx.sampler.clone())]));

            // Water.
            gfx.write_params(base)?;
            ctx.VSSetShader(&gfx.water_vs, None);
            ctx.PSSetShader(&gfx.water_ps, None);
            ctx.PSSetShaderResources(0, Some(&[Some(self.height_srv.clone())]));
            ctx.Draw(3, 0);

            if let Some((tex, draw)) = sprite {
                ctx.VSSetShader(&gfx.sprite_vs, None);
                ctx.PSSetShader(&gfx.sprite_ps, None);
                ctx.PSSetShaderResources(0, Some(&[Some(tex.srv.clone())]));

                // Reflection first (floaters only), then the character on top.
                if let Some(refl) = draw.reflection {
                    let mut p = *base;
                    p.sprite_rect = refl.rect;
                    p.sprite_uv = refl.uv;
                    p.sprite_misc = refl.misc;
                    gfx.write_params(&p)?;
                    ctx.Draw(4, 0);
                }
                let mut p = *base;
                p.sprite_rect = draw.body.rect;
                p.sprite_uv = draw.body.uv;
                p.sprite_misc = draw.body.misc;
                gfx.write_params(&p)?;
                ctx.Draw(4, 0);

                // Unbind so the next frame's water pass can't alias the atlas.
                ctx.PSSetShaderResources(0, Some(&[None]));
            }
        }
        Ok(())
    }

    pub fn present(&self) -> Result<()> {
        unsafe { self.swapchain.Present(1, DXGI_PRESENT(0)).ok().context("present") }
    }
}

#[derive(Clone, Copy)]
pub struct SpriteQuad {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub misc: [f32; 4],
}

pub struct SpriteDraw {
    pub body: SpriteQuad,
    pub reflection: Option<SpriteQuad>,
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        let hr = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                windows::Win32::Foundation::HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        if hr.is_ok() {
            if driver == D3D_DRIVER_TYPE_WARP {
                log::warn!("hardware D3D11 unavailable, using WARP software rasterizer");
            }
            return Ok((device.unwrap(), context.unwrap()));
        }
    }
    bail!("failed to create any D3D11 device")
}

fn create_rtv(device: &ID3D11Device, swapchain: &IDXGISwapChain1) -> Result<ID3D11RenderTargetView> {
    let back: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }.context("getting backbuffer")?;
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv))? };
    Ok(rtv.unwrap())
}

fn create_height_texture(
    device: &ID3D11Device,
    grid_w: u32,
    grid_h: u32,
) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: grid_w,
        Height: grid_h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R32_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    let tex = tex.unwrap();
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&tex, None, Some(&mut srv))? };
    Ok((tex, srv.unwrap()))
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob> {
    let entry_c = std::ffi::CString::new(entry)?;
    let target_c = std::ffi::CString::new(target)?;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let hr = unsafe {
        D3DCompile(
            SHADER_SRC.as_ptr() as *const _,
            SHADER_SRC.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry_c.as_ptr() as *const u8),
            PCSTR(target_c.as_ptr() as *const u8),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = hr {
        let msg = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        bail!("shader {entry} failed to compile: {e}\n{msg}");
    }
    Ok(code.unwrap())
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}
