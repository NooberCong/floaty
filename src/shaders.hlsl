// Floaty shaders. All passes output premultiplied alpha into a
// DXGI_ALPHA_MODE_PREMULTIPLIED composition swapchain, so the taskbar shows
// through wherever we draw little or nothing.

cbuffer Params : register(b0)
{
    float2 viewport;        // overlay size in pixels
    float  time;            // seconds
    float  wave_intensity;  // 0..2

    float3 shallow;         // surface water color (sRGB)
    float  opacity;         // base water opacity

    float3 deep;            // bottom water color (sRGB)
    float  _pad0;

    float4 sprite_rect;     // center x, center y, width, height (pixels)
    float4 sprite_uv;       // u0, v0, u1, v1 in the atlas
    float4 sprite_misc;     // tilt (radians), facing scale, waterline y px (0 = none), reflection
};

Texture2D<float> heightField : register(t0);
Texture2D atlas : register(t0); // sprite passes bind the atlas to the same slot
SamplerState linearClamp : register(s0);

// ---------------------------------------------------------------- water ----

struct WaterVsOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

WaterVsOut water_vs(uint id : SV_VertexID)
{
    // Fullscreen triangle.
    WaterVsOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = uv;
    return o;
}

// Ambient traveling waves keep the pool alive even when no ripples are active.
// Returns height; outputs d/dx via `slope`.
float ambient(float x, float y, out float slope)
{
    float a = sin(x * 0.021 + time * 1.35) * 0.14
            + sin(x * 0.0093 - time * 0.62 + y * 2.1) * 0.10
            + sin(x * 0.047 + time * 2.30) * 0.045;
    slope = (cos(x * 0.021 + time * 1.35) * 0.14 * 0.021
           + cos(x * 0.0093 - time * 0.62 + y * 2.1) * 0.10 * 0.0093
           + cos(x * 0.047 + time * 2.30) * 0.045 * 0.047) * viewport.x;
    return a;
}

float4 water_ps(WaterVsOut i) : SV_Target
{
    float2 texel = 1.0 / viewport; // heightfield is sampled in overlay UV space

    // Interactive ripple field + finite-difference slopes.
    float h  = heightField.Sample(linearClamp, i.uv);
    float hx = heightField.Sample(linearClamp, i.uv + float2(texel.x * 2, 0)) - h;
    float hy = heightField.Sample(linearClamp, i.uv + float2(0, texel.y * 2)) - h;

    float ambSlope;
    float amb = ambient(i.uv.x * viewport.x, i.uv.y, ambSlope) * wave_intensity;
    h += amb;
    float dhdx = hx * 18.0 + ambSlope * wave_intensity * 0.02;
    float dhdy = hy * 18.0;

    float3 N = normalize(float3(-dhdx, -dhdy, 1.0));

    // Depth gradient down the bar.
    float d = smoothstep(0.0, 1.0, i.uv.y);
    float3 col = lerp(shallow, deep, d);

    // Slope shading fakes refraction: light gathers on one side of a wave.
    col *= 1.0 + dhdx * 0.55 - d * 0.12;

    // Specular glints from a fixed key light.
    float3 L = normalize(float3(-0.42, -0.55, 0.72));
    float3 R = reflect(-L, N);
    float spec = pow(saturate(R.z), 90.0) * 0.9 + pow(saturate(R.z), 18.0) * 0.18;

    // Foam where ripples crest.
    float foam = smoothstep(0.30, 0.85, abs(h) * 1.35) * 0.22;

    // Bright waterline along the top few pixels of the pool.
    float waterline = exp2(-i.uv.y * viewport.y * 1.1) * 0.5;

    float alpha = saturate(opacity * (0.70 + 0.55 * d) + spec * 0.35 + foam * 0.4 + waterline * 0.25);
    col += (spec + foam + waterline) * float3(1, 1, 1);

    return float4(col * alpha, alpha); // premultiply
}

// --------------------------------------------------------------- sprite ----

struct SpriteVsOut
{
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;   // atlas uv
    float2 local : TEXCOORD1; // 0..1 within the quad (for reflection fade)
};

SpriteVsOut sprite_vs(uint id : SV_VertexID)
{
    // Triangle-strip quad: (0,0) (1,0) (0,1) (1,1).
    float2 corner = float2(id & 1, id >> 1);
    float2 centered = corner - 0.5;

    float2 size = sprite_rect.zw;
    float2 p = centered * float2(size.x * sprite_misc.y, size.y);

    float s = sin(sprite_misc.x), c = cos(sprite_misc.x);
    p = float2(p.x * c - p.y * s, p.x * s + p.y * c);
    p += sprite_rect.xy;

    SpriteVsOut o;
    o.pos = float4(p / viewport * float2(2, -2) + float2(-1, 1), 0, 1);
    o.uv = lerp(sprite_uv.xy, sprite_uv.zw, corner);
    o.local = corner;
    return o;
}

float4 sprite_ps(SpriteVsOut i) : SV_Target
{
    // "Sharp bilinear": snap to texel centers but keep a hair of filtering so
    // pixel art stays crisp at fractional scales without shimmering.
    float2 dim;
    atlas.GetDimensions(dim.x, dim.y);
    float2 px = i.uv * dim;
    float2 seam = floor(px + 0.5);
    float2 width = max(fwidth(px), 1e-5);
    px = seam + clamp((px - seam) / width, -0.5, 0.5);
    float4 tex = atlas.Sample(linearClamp, px / dim); // premultiplied already

    float reflection = sprite_misc.w;
    float depth = i.pos.y - sprite_misc.z; // px below the waterline

    // Body and reflection tile the water surface exactly: the body ends at
    // the waterline and its mirror starts there, over a shared soft edge —
    // no overlap, no gap, and the mirrored content stays continuous with the
    // visible body, regardless of where a character's waterline sits.
    float edge = max(sprite_rect.w * 0.05, 2.0);

    if (reflection > 0.5)
    {
        // Brightest at the contact line, dying off exponentially with depth
        // so the mirrored body reads as a sheen. Shimmer a touch.
        float fade = 0.30 * exp2(-depth / (sprite_rect.w * 0.15));
        fade *= 0.85 + 0.15 * sin(time * 3.0 + i.local.x * 9.0);
        tex *= fade * smoothstep(0.0, edge, depth);
    }
    else if (sprite_misc.z > 0.0)
    {
        // Floater body below its waterline is hidden by the water.
        tex *= 1.0 - smoothstep(0.0, edge, depth);
    }

    return tex;
}
