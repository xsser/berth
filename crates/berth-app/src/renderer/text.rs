//! Shaping and rasterization (DESIGN §8.1).
//!
//! - `cosmic_text::FontSystem` supplies system fonts and the fallback chain
//!   (PingFang for Han, Apple Color Emoji, …).
//! - Each terminal line is shaped with `ShapeLine`; glyph advances are
//!   **ignored**: every cluster is pinned to the cell of its first
//!   character, and a cluster spanning several cells (wide char, ZWJ emoji,
//!   flag) is drawn from its first cell. Glyphs *inside* a cluster keep their
//!   relative offsets (combining marks).
//! - Shaping results are cached by a hash of (text, font-attribute runs);
//!   colors do not affect shaping and are applied per cell later.
//! - `swash` rasterizes into alpha masks (outlines) or premultiplied RGBA
//!   (color bitmaps / COLR), packed by `atlas`.

use berth_core::{char_cells, CellFlags, LineSnapshot, StyleTable};
use cosmic_text::{
    fontdb, Attrs, AttrsList, CacheKeyFlags, Family, FontSystem, ShapeLine, Shaping, Weight,
};
use rustc_hash::{FxHashMap, FxHasher};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::{Angle, Format, Transform};

use super::atlas::RasterGlyph;
use super::metrics::{CellMetrics, FaceMetrics};
use super::sprites;

/// Font selection derived from cell flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontAttrs {
    pub bold: bool,
    pub italic: bool,
}

impl FontAttrs {
    pub fn from_flags(flags: CellFlags) -> Self {
        Self {
            bold: flags.contains(CellFlags::BOLD),
            italic: flags.contains(CellFlags::ITALIC),
        }
    }
}

/// What to rasterize for one glyph; also the atlas key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GlyphKey {
    Font {
        font: fontdb::ID,
        weight: u16,
        glyph: u16,
        size_bits: u32,
        fake_italic: bool,
    },
    /// Procedural box-drawing / block / Powerline cell.
    Sprite { ch: char },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    /// Cell column of the cluster's first character.
    pub col: u16,
    pub key: GlyphKey,
    /// Pixel offset from the cell's pen origin (x right, y up).
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Default)]
pub struct ShapedLine {
    pub glyphs: Vec<ShapedGlyph>,
}

/// Fallback statistics for the evidence log.
#[derive(Debug, Default, Clone)]
pub struct FontUsage {
    /// Glyphs shaped per font face.
    pub per_font: FxHashMap<fontdb::ID, u32>,
    /// `.notdef` glyphs (tofu) that are not drawn procedurally.
    pub missing: u32,
    pub missing_chars: Vec<char>,
    pub sprites: u32,
}

pub struct TextSystem {
    fs: FontSystem,
    family: String,
    primary: fontdb::ID,
    /// Faces belonging to the primary family (regular/bold/italic…).
    primary_faces: Vec<fontdb::ID>,
    font_px: f32,
    cell_w: f32,
    cache: FxHashMap<u64, Arc<ShapedLine>>,
    scale: ScaleContext,
    usage: FontUsage,
    text: String,
}

const SHAPE_CACHE_LIMIT: usize = 4096;

fn attrs_for(family: &str, fa: FontAttrs) -> Attrs<'_> {
    Attrs::new()
        .family(Family::Name(family))
        .weight(if fa.bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        })
        .style(if fa.italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        })
}

fn query(db: &fontdb::Database, family: &str) -> Option<fontdb::ID> {
    db.query(&fontdb::Query {
        families: &[Family::Name(family)],
        weight: Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    })
}

impl TextSystem {
    /// Loads system fonts (slow: do once). Falls back to SF Mono / Menlo
    /// when `family` is not installed.
    pub fn new(family: &str) -> anyhow::Result<Self> {
        let fs = FontSystem::new();
        let candidates = [family, "SF Mono", "Menlo", "Monaco", "Courier New"];
        let (family, primary) = candidates
            .iter()
            .find_map(|f| query(fs.db(), f).map(|id| (f.to_string(), id)))
            .ok_or_else(|| anyhow::anyhow!("no usable monospace font (tried {candidates:?})"))?;
        if !family.eq_ignore_ascii_case(candidates[0]) {
            tracing::warn!(requested = candidates[0], using = %family, "font family not found; falling back");
        }
        let primary_faces = fs
            .db()
            .faces()
            .filter(|f| {
                f.families
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(&family))
            })
            .map(|f| f.id)
            .collect();
        Ok(Self {
            fs,
            family,
            primary,
            primary_faces,
            font_px: 13.0,
            cell_w: 8.0,
            cache: FxHashMap::default(),
            scale: ScaleContext::new(),
            usage: FontUsage::default(),
            text: String::new(),
        })
    }

    pub fn family(&self) -> &str {
        &self.family
    }

    pub fn font_system(&mut self) -> &mut FontSystem {
        &mut self.fs
    }

    pub fn face_count(&self) -> usize {
        self.fs.db().len()
    }

    /// Metrics of the primary regular face (font units).
    pub fn face_metrics(&mut self) -> anyhow::Result<FaceMetrics> {
        let font = self
            .fs
            .get_font(self.primary, Weight::NORMAL)
            .ok_or_else(|| anyhow::anyhow!("cannot load primary font {}", self.family))?;
        let sw = font.as_swash();
        let m = sw.metrics(&[]);
        let charmap = sw.charmap();
        let gid = ['0', 'M', 'x']
            .iter()
            .map(|&c| charmap.map(c))
            .find(|&g| g != 0)
            .unwrap_or(0);
        let mut advance = sw.glyph_metrics(&[]).advance_width(gid);
        if advance <= 0.0 {
            advance = if m.average_width > 0.0 {
                m.average_width
            } else {
                m.units_per_em as f32 * 0.6
            };
        }
        Ok(FaceMetrics {
            units_per_em: m.units_per_em as f32,
            ascent: m.ascent,
            descent: m.descent,
            line_gap: m.leading,
            advance,
            underline_offset: m.underline_offset,
            underline_thickness: m.stroke_size,
            strikeout_offset: m.strikeout_offset,
            x_height: m.x_height,
        })
    }

    /// New pixel size (scale factor change): drops shaped lines.
    pub fn set_geometry(&mut self, font_px: f32, cell_w: f32) {
        if self.font_px != font_px || self.cell_w != cell_w {
            self.font_px = font_px;
            self.cell_w = cell_w;
            self.cache.clear();
        }
    }

    pub fn usage(&self) -> &FontUsage {
        &self.usage
    }

    /// Drop all shaped lines (benchmark: simulate a fully changed screen).
    pub fn clear_shape_cache(&mut self) {
        self.cache.clear();
    }

    pub fn font_name(&self, id: fontdb::ID) -> String {
        self.fs
            .db()
            .face(id)
            .map(|f| {
                f.families
                    .first()
                    .map(|(n, _)| n.clone())
                    .unwrap_or_else(|| f.post_script_name.clone())
            })
            .unwrap_or_else(|| format!("{id:?}"))
    }

    fn is_primary(&self, id: fontdb::ID) -> bool {
        self.primary_faces.contains(&id)
    }

    /// Shape one line. Cached by (text, bold/italic runs, pixel size).
    pub fn shape_line(&mut self, line: &LineSnapshot, styles: &StyleTable) -> Arc<ShapedLine> {
        // Font-attribute spans over the concatenated run text.
        self.text.clear();
        let mut spans: Vec<(std::ops::Range<usize>, FontAttrs)> = Vec::new();
        for run in &line.runs {
            let fa = FontAttrs::from_flags(styles.get(run.style).flags);
            let start = self.text.len();
            self.text.push_str(&run.text);
            match spans.last_mut() {
                Some((range, last)) if *last == fa => range.end = self.text.len(),
                _ => spans.push((start..self.text.len(), fa)),
            }
        }
        let mut h = FxHasher::default();
        self.text.hash(&mut h);
        for (range, fa) in &spans {
            range.hash(&mut h);
            fa.hash(&mut h);
        }
        let key = h.finish();
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let shaped = Arc::new(self.shape_uncached(&spans));
        if self.cache.len() >= SHAPE_CACHE_LIMIT {
            self.cache.clear();
        }
        self.cache.insert(key, shaped.clone());
        shaped
    }

    fn shape_uncached(&mut self, spans: &[(std::ops::Range<usize>, FontAttrs)]) -> ShapedLine {
        let text = std::mem::take(&mut self.text);
        let mut out = ShapedLine::default();
        if text.chars().all(|c| c == ' ') {
            self.text = text;
            return out;
        }
        // Byte offset of every char start → cell column (plus an end sentinel).
        let mut starts: Vec<(usize, u16)> = Vec::with_capacity(text.len() + 1);
        let mut col = 0u16;
        for (i, ch) in text.char_indices() {
            starts.push((i, col));
            col += char_cells(ch);
        }
        starts.push((text.len(), col));
        let col_at = |byte: usize| match starts.binary_search_by_key(&byte, |&(b, _)| b) {
            Ok(i) => starts[i].1,
            Err(i) => starts[i.saturating_sub(1)].1,
        };

        let mut list = AttrsList::new(&attrs_for(&self.family, FontAttrs::default()));
        for (range, fa) in spans {
            if *fa != FontAttrs::default() {
                list.add_span(range.clone(), &attrs_for(&self.family, *fa));
            }
        }
        let shape = ShapeLine::new(&mut self.fs, &text, &list, Shaping::Advanced, 8);

        let font_px = self.font_px;
        let mut cluster = usize::MAX;
        let mut pen = 0.0f32; // em units within the current cluster
        let mut cluster_scale = 1.0f32;
        let mut cluster_shift = 0.0f32;
        let mut cluster_skip = false;
        for span in &shape.spans {
            for word in &span.words {
                for g in &word.glyphs {
                    if g.start != cluster {
                        cluster = g.start;
                        pen = 0.0;
                        let first = text[g.start..].chars().next().unwrap_or(' ');
                        let end = g.end.max(g.start + first.len_utf8()).min(text.len());
                        let single = end == g.start + first.len_utf8();
                        let col = col_at(g.start);
                        let cells = col_at(end).saturating_sub(col).max(1);
                        cluster_skip = false;
                        if single && sprites::is_sprite(first) {
                            out.glyphs.push(ShapedGlyph {
                                col,
                                key: GlyphKey::Sprite { ch: first },
                                x: 0.0,
                                y: 0.0,
                            });
                            self.usage.sprites += 1;
                            cluster_skip = true;
                            continue;
                        }
                        if single && (first == ' ' || first == '\t') {
                            cluster_skip = true;
                            continue;
                        }
                        // Fit fallback glyphs into the cluster's cells: shrink when
                        // wider, centre when narrower. Primary-font glyphs are
                        // already on the cell pitch and stay untouched.
                        cluster_scale = 1.0;
                        cluster_shift = 0.0;
                        if !self.is_primary(g.font_id) {
                            let avail = f32::from(cells.min(2)) * self.cell_w;
                            let adv = g.x_advance * font_px;
                            if adv > avail * 1.05 && adv > 0.0 {
                                cluster_scale = avail / adv;
                            } else if adv > 0.0 && adv < avail {
                                cluster_shift = ((avail - adv) / 2.0).floor();
                            }
                        }
                    }
                    if cluster_skip {
                        continue;
                    }
                    if g.glyph_id == 0 {
                        self.usage.missing += 1;
                        if let Some(c) = text[g.start..].chars().next() {
                            if self.usage.missing_chars.len() < 32
                                && !self.usage.missing_chars.contains(&c)
                            {
                                self.usage.missing_chars.push(c);
                            }
                        }
                    }
                    *self.usage.per_font.entry(g.font_id).or_default() += 1;
                    let size = font_px * cluster_scale;
                    out.glyphs.push(ShapedGlyph {
                        col: col_at(g.start),
                        key: GlyphKey::Font {
                            font: g.font_id,
                            weight: g.font_weight.0,
                            glyph: g.glyph_id,
                            size_bits: size.to_bits(),
                            fake_italic: g.cache_key_flags.contains(CacheKeyFlags::FAKE_ITALIC),
                        },
                        x: cluster_shift + (pen + g.x_offset) * size,
                        y: g.y_offset * size,
                    });
                    pen += g.x_advance;
                }
            }
        }
        self.text = text;
        out
    }

    /// Rasterize a glyph for the atlas. `None` = nothing to draw.
    pub fn rasterize(&mut self, key: &GlyphKey, m: &CellMetrics) -> Option<RasterGlyph> {
        match *key {
            GlyphKey::Sprite { ch } => sprites::render(ch, m.cell_w, m.cell_h, m.box_thickness)
                .map(|data| RasterGlyph {
                    left: 0,
                    top: m.baseline as i32,
                    width: m.cell_w,
                    height: m.cell_h,
                    color: false,
                    data,
                }),
            GlyphKey::Font {
                font,
                weight,
                glyph,
                size_bits,
                fake_italic,
            } => {
                let font = self.fs.get_font(font, Weight(weight))?;
                let sw = font.as_swash();
                let mut builder = self
                    .scale
                    .builder(sw)
                    .size(f32::from_bits(size_bits))
                    .hint(false);
                let wght = swash::Tag::from_be_bytes(*b"wght");
                if let Some(axis) = sw.variations().find_by_tag(wght) {
                    let value = f32::from(weight).clamp(axis.min_value(), axis.max_value());
                    builder = builder
                        .normalized_coords(sw.variations().normalized_coords([(wght, value)]));
                }
                let mut scaler = builder.build();
                let image =
                    Render::new(&[
                        Source::ColorOutline(0),
                        Source::ColorBitmap(StrikeWith::BestFit),
                        Source::Outline,
                    ])
                    .format(Format::Alpha)
                    .transform(fake_italic.then(|| {
                        Transform::skew(Angle::from_degrees(14.0), Angle::from_degrees(0.0))
                    }))
                    .render(&mut scaler, glyph)?;
                let p = image.placement;
                if p.width == 0 || p.height == 0 {
                    return None;
                }
                let (color, data) = match image.content {
                    Content::Mask => (false, image.data),
                    Content::Color => {
                        let mut data = image.data;
                        // COLR layers are composited premultiplied by swash;
                        // PNG strikes (sbix, Apple Color Emoji) are straight alpha.
                        if matches!(image.source, Source::ColorBitmap(_) | Source::Bitmap(_)) {
                            premultiply(&mut data);
                        }
                        (true, data)
                    }
                    Content::SubpixelMask => {
                        // Not requested (Format::Alpha); reduce to coverage if it ever happens.
                        (
                            false,
                            image
                                .data
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|px| px[0].max(px[1]).max(px[2]))
                                .collect(),
                        )
                    }
                };
                Some(RasterGlyph {
                    left: p.left,
                    top: p.top,
                    width: p.width,
                    height: p.height,
                    color,
                    data,
                })
            }
        }
    }
}

/// Straight RGBA → premultiplied RGBA, in place.
pub fn premultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u16;
        for c in &mut px[..3] {
            *c = ((*c as u16 * a + 127) / 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_scales_color_by_alpha() {
        let mut px = [255, 128, 0, 128, 10, 20, 30, 255, 200, 200, 200, 0];
        premultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn font_attrs_follow_flags() {
        assert_eq!(
            FontAttrs::from_flags(CellFlags::BOLD | CellFlags::UNDERLINE),
            FontAttrs {
                bold: true,
                italic: false
            }
        );
        assert_eq!(
            FontAttrs::from_flags(CellFlags::ITALIC),
            FontAttrs {
                bold: false,
                italic: true
            }
        );
    }

    /// Uses the machine's installed fonts (SF Mono / PingFang / Apple Color
    /// Emoji on macOS), so it only runs there.
    #[cfg(target_os = "macos")]
    #[test]
    fn fixture_shapes_without_tofu_and_clusters_pin_to_first_cell() {
        use berth_core::StyleId;
        let mut ts = TextSystem::new("SF Mono").unwrap();
        ts.set_geometry(26.0, 16.0);
        let f = crate::fixture::Fixture::build();
        for line in &f.screen.lines {
            ts.shape_line(line, f.interner.table());
        }
        let usage = ts.usage().clone();
        assert_eq!(usage.missing, 0, "tofu for {:?}", usage.missing_chars);
        assert!(
            usage.sprites > 100,
            "box drawing is procedural: {}",
            usage.sprites
        );
        let fonts: Vec<String> = usage.per_font.keys().map(|id| ts.font_name(*id)).collect();
        assert!(fonts.iter().any(|n| n.contains("Emoji")), "{fonts:?}");
        assert!(
            fonts
                .iter()
                .any(|n| n.contains("PingFang") || n.contains("Hiragino")),
            "{fonts:?}"
        );

        // ZWJ family: one glyph in the first cell; unicode-width gives it 6 cells.
        let mut line = LineSnapshot::blank();
        line.push_str(
            "a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b",
            StyleId::DEFAULT,
        );
        let shaped = ts.shape_line(&line, f.interner.table());
        let cols: Vec<u16> = shaped.glyphs.iter().map(|g| g.col).collect();
        assert_eq!(cols, vec![0, 1, 7], "{:?}", shaped.glyphs);

        // Combining acute: base and mark share cell 0; next char in cell 1.
        let mut line = LineSnapshot::blank();
        line.push_str("e\u{301}x", StyleId::DEFAULT);
        let shaped = ts.shape_line(&line, f.interner.table());
        let cols: Vec<u16> = shaped.glyphs.iter().map(|g| g.col).collect();
        assert!(
            cols == vec![0, 1] || cols == vec![0, 0, 1],
            "{:?}",
            shaped.glyphs
        );
    }
}
