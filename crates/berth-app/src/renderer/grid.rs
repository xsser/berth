//! Terminal grid renderer (DESIGN §8.1).
//!
//! Each frame is three instanced layers drawn in one render pass:
//! 1. background quads (cell backgrounds merged per run, selection, block cursor),
//! 2. glyph quads sampled 1:1 from the atlas (mask or color page),
//! 3. decoration quads (underline variants, strikeout, beam/underline/hollow
//!    cursor, IME preedit underline and caret).

use berth_core::{
    char_cells, CellFlags, CursorShape, LineSnapshot, ScreenSnapshot, Style, StyleId, StyleTable,
    TermModes,
};
use bytemuck::{Pod, Zeroable};
use std::mem::size_of;

use super::atlas::{AtlasEntry, AtlasFull, AtlasKind, GlyphAtlas};
use super::metrics::{CellMetrics, FaceMetrics};
use super::text::{GlyphKey, ShapedGlyph, TextSystem};
use crate::config::FontConfig;
use crate::ime::Preedit;
use crate::selection::SelectionSpans;
use crate::theme::{mix, rgba, CellColors, Rgb, Theme};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct QuadInstance {
    /// x, y, w, h in physical pixels.
    pub rect: [f32; 4],
    /// Straight (non-premultiplied) RGBA.
    pub color: [f32; 4],
    /// kind, thickness, period, amplitude.
    pub params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct GlyphInstance {
    pub rect: [f32; 4],
    /// Atlas texel rect.
    pub uv: [f32; 4],
    pub color: [f32; 4],
    /// x: 0 = mask page, 1 = color page.
    pub kind: [u32; 4],
}

const QUAD_SOLID: f32 = 0.0;
const QUAD_UNDERCURL: f32 = 1.0;
const QUAD_DOTTED: f32 = 2.0;
const QUAD_DASHED: f32 = 3.0;
const QUAD_HOLLOW: f32 = 4.0;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

/// Everything the grid needs for one frame.
pub struct FrameInput<'a> {
    pub screen: &'a ScreenSnapshot,
    pub styles: &'a StyleTable,
    pub theme: &'a Theme,
    /// Selected cells of the visible rows.
    pub selection: Option<&'a SelectionSpans>,
    /// Cursor opacity from the blink animation (0 hidden … 1 solid).
    pub cursor_alpha: f32,
    pub focused: bool,
    pub preedit: Option<&'a Preedit>,
}

/// Where the grid sits in the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridLayout {
    /// Top-left of cell (0, 0) in physical pixels.
    pub origin: [f32; 2],
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PrepareStats {
    pub bg_quads: u32,
    pub glyphs: u32,
    pub deco_quads: u32,
    /// Glyphs rasterized this frame (atlas misses).
    pub rasterized: u32,
    pub atlas_rebuilds: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Underline {
    Single,
    Double,
    Curl,
    Dotted,
    Dashed,
}

fn underline_kind(flags: CellFlags) -> Option<Underline> {
    if flags.contains(CellFlags::UNDERCURL) {
        Some(Underline::Curl)
    } else if flags.contains(CellFlags::DOUBLE_UNDERLINE) {
        Some(Underline::Double)
    } else if flags.contains(CellFlags::DOTTED_UNDERLINE) {
        Some(Underline::Dotted)
    } else if flags.contains(CellFlags::DASHED_UNDERLINE) {
        Some(Underline::Dashed)
    } else if flags.contains(CellFlags::UNDERLINE) {
        Some(Underline::Single)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Cell {
    style: Style,
    /// First cell of a double-width character.
    wide: bool,
}

/// Placement of the IME preedit on the cursor row.
struct PreeditLayout {
    row: usize,
    start: usize,
    cells: usize,
    caret: Option<usize>,
    line: LineSnapshot,
}

fn preedit_layout(p: &Preedit, row: usize, col: usize, cols: usize) -> PreeditLayout {
    let mut line = LineSnapshot::blank();
    line.push_str(&p.text, StyleId::DEFAULT);
    let cells = (line.cells() as usize).min(cols);
    let start = col.min(cols.saturating_sub(cells));
    let caret = p.cursor.map(|(s, _)| {
        p.text[..s]
            .chars()
            .map(|c| char_cells(c) as usize)
            .sum::<usize>()
            .min(cells)
    });
    PreeditLayout {
        row,
        start,
        cells,
        caret,
        line,
    }
}

struct Layer<T> {
    cpu: Vec<T>,
    gpu: wgpu::Buffer,
    capacity: usize,
    label: &'static str,
}

impl<T: Pod> Layer<T> {
    fn buffer(device: &wgpu::Device, label: &'static str, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: (capacity * size_of::<T>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn new(device: &wgpu::Device, label: &'static str) -> Self {
        let capacity = 1024;
        Self {
            cpu: Vec::with_capacity(capacity),
            gpu: Self::buffer(device, label, capacity),
            capacity,
            label,
        }
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        if self.cpu.len() > self.capacity {
            self.capacity = self.cpu.len().next_power_of_two();
            self.gpu = Self::buffer(device, self.label, self.capacity);
        }
        if !self.cpu.is_empty() {
            queue.write_buffer(&self.gpu, 0, bytemuck::cast_slice(&self.cpu));
        }
    }

    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, pipeline: &wgpu::RenderPipeline) {
        if self.cpu.is_empty() {
            return;
        }
        pass.set_pipeline(pipeline);
        pass.set_vertex_buffer(0, self.gpu.slice(..));
        pass.draw(0..4, 0..self.cpu.len() as u32);
    }
}

pub struct GridRenderer {
    pub text: TextSystem,
    face: FaceMetrics,
    metrics: CellMetrics,
    font: FontConfig,
    atlas: GlyphAtlas<GlyphKey>,
    globals: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    bind_version: u64,
    quad_pipeline: wgpu::RenderPipeline,
    glyph_pipeline: wgpu::RenderPipeline,
    bg: Layer<QuadInstance>,
    glyphs: Layer<GlyphInstance>,
    deco: Layer<QuadInstance>,
    cells: Vec<Cell>,
    colors: Vec<CellColors>,
}

fn bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    atlas: &GlyphAtlas<GlyphKey>,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("grid"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(atlas.view(AtlasKind::Mask)),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(atlas.view(AtlasKind::Color)),
            },
        ],
    })
}

impl GridRenderer {
    /// `scale` is the window scale factor (physical px per point).
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        font: &FontConfig,
        scale: f32,
    ) -> anyhow::Result<Self> {
        let mut text = TextSystem::new(&font.family)?;
        let face = text.face_metrics()?;
        let metrics = CellMetrics::compute(&face, font.size * scale);
        text.set_geometry(metrics.font_px, metrics.cell_w as f32);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders.wgsl").into()),
        });
        let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("grid"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1),
                texture_entry(2),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let quad_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        let glyph_attrs =
            wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Uint32x4];
        let pipeline =
            |label: &str, vs: &str, fs: &str, stride: usize, attrs: &[wgpu::VertexAttribute]| {
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some(vs),
                        compilation_options: Default::default(),
                        buffers: &[Some(wgpu::VertexBufferLayout {
                            array_stride: stride as u64,
                            step_mode: wgpu::VertexStepMode::Instance,
                            attributes: attrs,
                        })],
                    },
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleStrip,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some(fs),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState {
                            format,
                            blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                    }),
                    multiview_mask: None,
                    cache: None,
                })
            };
        let quad_pipeline = pipeline(
            "grid-quads",
            "vs_quad",
            "fs_quad",
            size_of::<QuadInstance>(),
            &quad_attrs,
        );
        let glyph_pipeline = pipeline(
            "grid-glyphs",
            "vs_glyph",
            "fs_glyph",
            size_of::<GlyphInstance>(),
            &glyph_attrs,
        );

        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid-globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let atlas = GlyphAtlas::new(device);
        let bind_group = bind_group(device, &bind_layout, &globals, &atlas);
        let bind_version = atlas.version();
        Ok(Self {
            text,
            face,
            metrics,
            font: font.clone(),
            atlas,
            globals,
            bind_layout,
            bind_group,
            bind_version,
            quad_pipeline,
            glyph_pipeline,
            bg: Layer::new(device, "grid-bg"),
            glyphs: Layer::new(device, "grid-glyphs"),
            deco: Layer::new(device, "grid-deco"),
            cells: Vec::new(),
            colors: Vec::new(),
        })
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn font(&self) -> &FontConfig {
        &self.font
    }

    /// Recompute cell metrics for a new scale factor; drops cached glyphs.
    pub fn set_scale(&mut self, scale: f32) {
        let metrics = CellMetrics::compute(&self.face, self.font.size * scale);
        if metrics != self.metrics {
            self.metrics = metrics;
            self.text
                .set_geometry(metrics.font_px, metrics.cell_w as f32);
            self.atlas.core.reset();
        }
    }

    /// (cached glyphs, mask page size, color page size, rebuild count).
    pub fn atlas_summary(&self) -> (usize, u32, u32, u64) {
        let core = &self.atlas.core;
        (
            core.len(),
            core.size(AtlasKind::Mask),
            core.size(AtlasKind::Color),
            core.generation(),
        )
    }

    /// Top-left pixel and size of the IME caret cell (for the candidate window).
    pub fn ime_caret_rect(&self, input: &FrameInput, layout: &GridLayout) -> [f32; 4] {
        let m = self.metrics;
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let c = &input.screen.cursor;
        let cols = layout.cols.min(input.screen.cols) as usize;
        let mut col = c.col as usize;
        if let Some(p) = input.preedit {
            let pl = preedit_layout(p, c.row as usize, col, cols);
            col = pl.start + pl.caret.unwrap_or(pl.cells);
        }
        [
            layout.origin[0] + col as f32 * cw,
            layout.origin[1] + c.row as f32 * ch,
            cw,
            ch,
        ]
    }

    /// Build and upload this frame's instances.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        input: &FrameInput,
        layout: &GridLayout,
        viewport: [u32; 2],
    ) -> PrepareStats {
        let mut stats = PrepareStats::default();
        for attempt in 0..3 {
            match self.build(input, layout, &mut stats) {
                Ok(()) => break,
                Err(AtlasFull(kind)) => {
                    stats.atlas_rebuilds += 1;
                    if attempt == 0 {
                        tracing::info!(
                            ?kind,
                            glyphs = self.atlas.core.len(),
                            "glyph atlas full; rebuilding"
                        );
                        self.atlas.core.reset();
                    } else if self.atlas.core.grow(kind) {
                        tracing::info!(
                            ?kind,
                            size = self.atlas.core.size(kind),
                            "glyph atlas grown"
                        );
                    } else {
                        tracing::warn!(?kind, "glyph atlas exhausted; frame drawn partially");
                        break;
                    }
                }
            }
        }
        self.atlas.flush(device, queue);
        if self.atlas.version() != self.bind_version {
            self.bind_group = bind_group(device, &self.bind_layout, &self.globals, &self.atlas);
            self.bind_version = self.atlas.version();
        }
        let globals = Globals {
            viewport: [viewport[0] as f32, viewport[1] as f32],
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
        self.bg.upload(device, queue);
        self.glyphs.upload(device, queue);
        self.deco.upload(device, queue);
        stats.bg_quads = self.bg.cpu.len() as u32;
        stats.glyphs = self.glyphs.cpu.len() as u32;
        stats.deco_quads = self.deco.cpu.len() as u32;
        stats
    }

    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_bind_group(0, &self.bind_group, &[]);
        self.bg.draw(pass, &self.quad_pipeline);
        self.glyphs.draw(pass, &self.glyph_pipeline);
        self.deco.draw(pass, &self.quad_pipeline);
    }

    fn glyph_entry(
        &mut self,
        key: &GlyphKey,
        stats: &mut PrepareStats,
    ) -> Result<Option<AtlasEntry>, AtlasFull> {
        if let Some(entry) = self.atlas.core.get(key) {
            return Ok(entry);
        }
        let raster = self.text.rasterize(key, &self.metrics);
        stats.rasterized += 1;
        self.atlas.core.insert(*key, raster)
    }

    fn push_glyph(
        &mut self,
        g: &ShapedGlyph,
        pen: [f32; 2],
        fg: Rgb,
        alpha: f32,
        stats: &mut PrepareStats,
    ) -> Result<(), AtlasFull> {
        let Some(e) = self.glyph_entry(&g.key, stats)? else {
            return Ok(());
        };
        let x = (pen[0] + g.x).round() + e.left as f32;
        let y = (pen[1] - g.y).round() - e.top as f32;
        let color = if e.kind == AtlasKind::Color {
            [1.0, 1.0, 1.0, alpha]
        } else {
            rgba(fg, alpha)
        };
        self.glyphs.cpu.push(GlyphInstance {
            rect: [x, y, e.w as f32, e.h as f32],
            uv: [e.x as f32, e.y as f32, e.w as f32, e.h as f32],
            color,
            kind: [e.kind as u32, 0, 0, 0],
        });
        Ok(())
    }

    fn quad(layer: &mut Vec<QuadInstance>, rect: [f32; 4], color: [f32; 4], params: [f32; 4]) {
        if rect[2] > 0.0 && rect[3] > 0.0 && color[3] > 0.0 {
            layer.push(QuadInstance {
                rect,
                color,
                params,
            });
        }
    }

    fn push_underline(&mut self, kind: Underline, x: f32, y: f32, w: f32, color: [f32; 4]) {
        let m = self.metrics;
        let th = m.underline_thickness as f32;
        let ch = m.cell_h as f32;
        let top = y + m.underline_top as f32;
        let deco = &mut self.deco.cpu;
        match kind {
            Underline::Single => {
                Self::quad(deco, [x, top, w, th], color, [QUAD_SOLID, th, 0.0, 0.0])
            }
            Underline::Double => {
                let t0 = if m.underline_top as f32 + 3.0 * th > ch {
                    y + ch - 3.0 * th
                } else {
                    top
                };
                Self::quad(deco, [x, t0, w, th], color, [QUAD_SOLID, th, 0.0, 0.0]);
                Self::quad(
                    deco,
                    [x, t0 + 2.0 * th, w, th],
                    color,
                    [QUAD_SOLID, th, 0.0, 0.0],
                );
            }
            Underline::Curl => {
                let amp = (th * 1.2).max(1.5);
                let h = (2.0 * (amp + th)).ceil();
                let center = (top + th / 2.0).min(y + ch - h / 2.0);
                let period = m.cell_w as f32;
                Self::quad(
                    deco,
                    [x, (center - h / 2.0).round(), w, h],
                    color,
                    [QUAD_UNDERCURL, th, period, amp],
                );
            }
            Underline::Dotted => Self::quad(
                deco,
                [x, top, w, th],
                color,
                [QUAD_DOTTED, th, 2.0 * th, 0.0],
            ),
            Underline::Dashed => Self::quad(
                deco,
                [x, top, w, th],
                color,
                [QUAD_DASHED, th, (m.cell_w as f32 / 2.0).max(2.0), 0.0],
            ),
        }
    }

    fn build(
        &mut self,
        input: &FrameInput,
        layout: &GridLayout,
        stats: &mut PrepareStats,
    ) -> Result<(), AtlasFull> {
        self.bg.cpu.clear();
        self.glyphs.cpu.clear();
        self.deco.cpu.clear();
        let m = self.metrics;
        let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);
        let [ox, oy] = layout.origin;
        let screen = input.screen;
        let theme = input.theme;
        let cols = layout.cols.min(screen.cols) as usize;
        let rows = (layout.rows.min(screen.rows) as usize).min(screen.lines.len());
        if cols == 0 || rows == 0 {
            return Ok(());
        }

        let cur = screen.cursor;
        let cursor_on = cur.visible
            && screen.modes.contains(TermModes::SHOW_CURSOR)
            && cur.shape != CursorShape::Hidden
            && (cur.row as usize) < rows
            && (cur.col as usize) < cols;
        let preedit = input
            .preedit
            .filter(|p| !p.text.is_empty() && (cur.row as usize) < rows)
            .map(|p| preedit_layout(p, cur.row as usize, (cur.col as usize).min(cols - 1), cols));
        let cursor_rgba = |alpha: f32| rgba(theme.cursor, alpha);

        for row in 0..rows {
            let line = &screen.lines[row];
            let y = oy + row as f32 * ch;

            // Cells: style per column, wide characters marked on their first cell.
            self.cells.clear();
            self.cells.resize(cols, Cell::default());
            let mut col = 0usize;
            'runs: for run in &line.runs {
                let style = input.styles.get(run.style);
                for c in run.text.chars() {
                    let w = char_cells(c) as usize;
                    if w == 0 {
                        continue;
                    }
                    if col >= cols {
                        break 'runs;
                    }
                    self.cells[col] = Cell {
                        style,
                        wide: w == 2,
                    };
                    if w == 2 && col + 1 < cols {
                        self.cells[col + 1] = Cell { style, wide: false };
                    }
                    col += w;
                }
            }
            self.colors.clear();
            for (c, cell) in self.cells.iter().enumerate() {
                let selected = input
                    .selection
                    .is_some_and(|s| s.contains(row as u16, c as u16));
                self.colors.push(theme.resolve(&cell.style, selected));
            }

            // Backgrounds, merged into runs of equal color.
            let mut c = 0;
            while c < cols {
                if self.colors[c].bg_is_default {
                    c += 1;
                    continue;
                }
                let start = c;
                let bg = self.colors[c].bg;
                while c < cols && !self.colors[c].bg_is_default && self.colors[c].bg == bg {
                    c += 1;
                }
                Self::quad(
                    &mut self.bg.cpu,
                    [ox + start as f32 * cw, y, (c - start) as f32 * cw, ch],
                    rgba(bg, 1.0),
                    [QUAD_SOLID; 4],
                );
            }

            // Cursor (hidden while an IME composition is shown).
            let mut block: Option<(usize, usize)> = None;
            if cursor_on && row == cur.row as usize && preedit.is_none() && input.cursor_alpha > 0.0
            {
                let cc = cur.col as usize;
                let span = if self.cells[cc].wide { 2 } else { 1 };
                let x = ox + cc as f32 * cw;
                let width = span as f32 * cw;
                let t = m.cursor_thickness as f32;
                let color = cursor_rgba(input.cursor_alpha);
                let shape = if input.focused {
                    cur.shape
                } else {
                    CursorShape::HollowBlock
                };
                match shape {
                    CursorShape::Block => {
                        Self::quad(&mut self.bg.cpu, [x, y, width, ch], color, [QUAD_SOLID; 4]);
                        block = Some((cc, span));
                    }
                    CursorShape::Beam => {
                        Self::quad(&mut self.deco.cpu, [x, y, t, ch], color, [QUAD_SOLID; 4])
                    }
                    CursorShape::Underline => Self::quad(
                        &mut self.deco.cpu,
                        [x, y + ch - t, width, t],
                        color,
                        [QUAD_SOLID; 4],
                    ),
                    CursorShape::HollowBlock => Self::quad(
                        &mut self.deco.cpu,
                        [x, y, width, ch],
                        color,
                        [QUAD_HOLLOW, t, 0.0, 0.0],
                    ),
                    CursorShape::Hidden => {}
                }
            }

            // Glyphs.
            let shaped = self.text.shape_line(line, input.styles);
            let baseline = y + m.baseline as f32;
            for g in &shaped.glyphs {
                let gc = g.col as usize;
                if gc >= cols {
                    continue;
                }
                if let Some(p) = &preedit {
                    if p.row == row && gc >= p.start && gc < p.start + p.cells {
                        continue;
                    }
                }
                let colors = self.colors[gc];
                if colors.fg_alpha <= 0.0 {
                    continue;
                }
                let mut fg = colors.fg;
                if let Some((bc, span)) = block {
                    if gc >= bc && gc < bc + span {
                        fg = mix(fg, theme.cursor_text, input.cursor_alpha);
                    }
                }
                self.push_glyph(
                    g,
                    [ox + gc as f32 * cw, baseline],
                    fg,
                    colors.fg_alpha,
                    stats,
                )?;
            }

            // Underlines (merged per kind + color), then strikeout.
            let mut c = 0;
            while c < cols {
                let kind = underline_kind(self.cells[c].style.flags);
                let colors = self.colors[c];
                let Some(k) = kind.filter(|_| colors.fg_alpha > 0.0) else {
                    c += 1;
                    continue;
                };
                let start = c;
                while c < cols
                    && underline_kind(self.cells[c].style.flags) == kind
                    && self.colors[c].underline == colors.underline
                {
                    c += 1;
                }
                self.push_underline(
                    k,
                    ox + start as f32 * cw,
                    y,
                    (c - start) as f32 * cw,
                    rgba(colors.underline, 1.0),
                );
            }
            let mut c = 0;
            while c < cols {
                let colors = self.colors[c];
                if !self.cells[c].style.flags.contains(CellFlags::STRIKEOUT)
                    || colors.fg_alpha <= 0.0
                {
                    c += 1;
                    continue;
                }
                let start = c;
                while c < cols
                    && self.cells[c].style.flags.contains(CellFlags::STRIKEOUT)
                    && self.colors[c].fg == colors.fg
                {
                    c += 1;
                }
                let th = m.strikeout_thickness as f32;
                Self::quad(
                    &mut self.deco.cpu,
                    [
                        ox + start as f32 * cw,
                        y + m.strikeout_top as f32,
                        (c - start) as f32 * cw,
                        th,
                    ],
                    rgba(colors.fg, 1.0),
                    [QUAD_SOLID; 4],
                );
            }
        }

        // IME preedit overlay: opaque background, glyphs, underline, caret.
        if let Some(p) = preedit {
            let y = oy + p.row as f32 * ch;
            let x0 = ox + p.start as f32 * cw;
            let width = p.cells as f32 * cw;
            Self::quad(
                &mut self.bg.cpu,
                [x0, y, width, ch],
                rgba(theme.background, 1.0),
                [QUAD_SOLID; 4],
            );
            let shaped = self.text.shape_line(&p.line, input.styles);
            for g in &shaped.glyphs {
                if (g.col as usize) < p.cells {
                    self.push_glyph(
                        g,
                        [x0 + g.col as f32 * cw, y + m.baseline as f32],
                        theme.foreground,
                        1.0,
                        stats,
                    )?;
                }
            }
            let th = m.underline_thickness as f32;
            Self::quad(
                &mut self.deco.cpu,
                [x0, y + m.underline_top as f32, width, th],
                rgba(theme.foreground, 1.0),
                [QUAD_SOLID; 4],
            );
            if let Some(caret) = p.caret {
                let t = m.cursor_thickness as f32;
                Self::quad(
                    &mut self.deco.cpu,
                    [x0 + caret as f32 * cw, y, t, ch],
                    cursor_rgba(1.0),
                    [QUAD_SOLID; 4],
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underline_priority() {
        assert_eq!(
            underline_kind(CellFlags::UNDERLINE | CellFlags::UNDERCURL),
            Some(Underline::Curl)
        );
        assert_eq!(
            underline_kind(CellFlags::DOUBLE_UNDERLINE | CellFlags::UNDERLINE),
            Some(Underline::Double)
        );
        assert_eq!(underline_kind(CellFlags::BOLD), None);
    }

    #[test]
    fn preedit_is_clamped_to_the_row() {
        let p = Preedit {
            text: "中文输入".into(),
            cursor: Some((6, 6)),
        };
        let l = preedit_layout(&p, 3, 118, 120);
        assert_eq!(l.cells, 8);
        assert_eq!(l.start, 112, "shifted left to fit");
        assert_eq!(l.caret, Some(4));
        let l = preedit_layout(&p, 3, 10, 120);
        assert_eq!(l.start, 10);
    }

    #[test]
    fn instance_layouts_are_16_byte_aligned() {
        assert_eq!(size_of::<QuadInstance>(), 48);
        assert_eq!(size_of::<GlyphInstance>(), 64);
    }
}
