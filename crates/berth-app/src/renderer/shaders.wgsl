// Grid renderer shaders: instanced quads for cell backgrounds / decorations
// and atlas-sampled glyphs. Coordinates are physical pixels, origin top-left.
// Output is premultiplied alpha in the surface's (gamma-encoded) space.

struct Globals {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var mask_tex: texture_2d<f32>;
@group(0) @binding(2) var color_tex: texture_2d<f32>;

// Triangle-strip corner for vertex 0..3: (0,0) (1,0) (0,1) (1,1).
fn corner(vi: u32) -> vec2<f32> {
    return vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
}

fn to_clip(p: vec2<f32>) -> vec4<f32> {
    let n = p / globals.viewport * 2.0 - vec2<f32>(1.0, 1.0);
    return vec4<f32>(n.x, -n.y, 0.0, 1.0);
}

// ---------------------------------------------------------------- quads

const QUAD_SOLID: u32 = 0u;
const QUAD_UNDERCURL: u32 = 1u;
const QUAD_DOTTED: u32 = 2u;
const QUAD_DASHED: u32 = 3u;
const QUAD_HOLLOW: u32 = 4u;

struct QuadOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,
    @location(2) @interpolate(flat) params: vec4<f32>,
    @location(3) @interpolate(flat) size: vec2<f32>,
};

@vertex
fn vs_quad(
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4<f32>,
    @location(1) color: vec4<f32>,
    @location(2) params: vec4<f32>,
) -> QuadOut {
    let c = corner(vi);
    var out: QuadOut;
    out.pos = to_clip(rect.xy + c * rect.zw);
    out.color = color;
    out.local = c * rect.zw;
    out.params = params;
    out.size = rect.zw;
    return out;
}

@fragment
fn fs_quad(in: QuadOut) -> @location(0) vec4<f32> {
    let kind = u32(in.params.x);
    let thickness = in.params.y;
    let period = max(in.params.z, 1.0);
    // Absolute framebuffer x keeps patterns continuous across cells.
    let px = in.pos.x;
    var coverage = 1.0;
    if kind == QUAD_UNDERCURL {
        let amp = in.params.w;
        let k = 6.2831853 / period;
        let center = in.size.y * 0.5;
        let y = center + amp * sin(px * k);
        let slope = amp * k * cos(px * k);
        let d = abs(in.local.y - y) / sqrt(1.0 + slope * slope);
        coverage = clamp(thickness * 0.5 + 0.5 - d, 0.0, 1.0);
    } else if kind == QUAD_DOTTED {
        coverage = select(0.0, 1.0, fract(px / period) < 0.5);
    } else if kind == QUAD_DASHED {
        coverage = select(0.0, 1.0, fract(px / period) < 0.6);
    } else if kind == QUAD_HOLLOW {
        let l = in.local;
        let edge = min(min(l.x, in.size.x - l.x), min(l.y, in.size.y - l.y));
        coverage = select(0.0, 1.0, edge < thickness);
    }
    let a = in.color.a * coverage;
    return vec4<f32>(in.color.rgb * a, a);
}

// ---------------------------------------------------------------- glyphs

struct GlyphOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs_glyph(
    @builtin(vertex_index) vi: u32,
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) kind: vec4<u32>,
) -> GlyphOut {
    let c = corner(vi);
    var out: GlyphOut;
    out.pos = to_clip(rect.xy + c * rect.zw);
    // Atlas texel coordinates; quads are pixel-aligned so this is 1:1.
    out.uv = uv.xy + c * uv.zw;
    out.color = color;
    out.kind = kind.x;
    return out;
}

@fragment
fn fs_glyph(in: GlyphOut) -> @location(0) vec4<f32> {
    let texel = vec2<i32>(floor(in.uv));
    if in.kind == 1u {
        // Premultiplied color emoji; `color.a` fades it (cursor blink, dim).
        return textureLoad(color_tex, texel, 0) * in.color.a;
    }
    let a = textureLoad(mask_tex, texel, 0).r * in.color.a;
    return vec4<f32>(in.color.rgb * a, a);
}
