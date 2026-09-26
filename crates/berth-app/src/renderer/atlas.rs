//! Glyph atlas: an R8 alpha-mask page for ordinary glyphs and an RGBA8
//! (premultiplied) page for color emoji, packed with `etagere`.
//!
//! `AtlasCore` is the CPU side (allocation, key → entry map, pending uploads)
//! and is unit-tested without a GPU. `GlyphAtlas` owns the wgpu textures.
//!
//! Policy when a page is full ("满了重建"): the caller resets the whole atlas
//! and rebuilds the frame, re-rasterizing only glyphs still visible. If one
//! frame alone does not fit, the page is doubled (up to the device limit).

use etagere::{size2, AtlasAllocator};
use rustc_hash::FxHashMap;
use std::hash::Hash;

/// Transparent gap between neighbouring glyphs.
const PAD: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AtlasKind {
    Mask = 0,
    Color = 1,
}

/// A rasterized glyph ready for upload.
#[derive(Clone, Debug, PartialEq)]
pub struct RasterGlyph {
    /// Offset of the bitmap's left edge from the pen position (px).
    pub left: i32,
    /// Offset of the bitmap's top edge above the baseline (px).
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// RGBA8 premultiplied when true, R8 alpha otherwise.
    pub color: bool,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasEntry {
    pub kind: AtlasKind,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub left: i32,
    pub top: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub kind: AtlasKind,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtlasFull(pub AtlasKind);

struct Page {
    alloc: AtlasAllocator,
    size: u32,
}

impl Page {
    fn new(size: u32) -> Self {
        Self {
            alloc: AtlasAllocator::new(size2(size as i32, size as i32)),
            size,
        }
    }
}

pub struct AtlasCore<K> {
    pages: [Page; 2],
    max_size: u32,
    /// `None` = glyph known to have no pixels (spaces, empty outlines).
    entries: FxHashMap<K, Option<AtlasEntry>>,
    uploads: Vec<Upload>,
    generation: u64,
}

impl<K: Copy + Eq + Hash> AtlasCore<K> {
    pub fn new(initial: u32, max_size: u32) -> Self {
        let initial = initial.min(max_size);
        Self {
            pages: [Page::new(initial), Page::new(initial)],
            max_size,
            entries: FxHashMap::default(),
            uploads: Vec::new(),
            generation: 0,
        }
    }

    /// `Some(entry)` if the key was seen before (entry may be `None` for
    /// empty glyphs), `None` if it must be rasterized.
    pub fn get(&self, key: &K) -> Option<Option<AtlasEntry>> {
        self.entries.get(key).copied()
    }

    pub fn insert(
        &mut self,
        key: K,
        glyph: Option<RasterGlyph>,
    ) -> Result<Option<AtlasEntry>, AtlasFull> {
        let Some(g) = glyph.filter(|g| g.width > 0 && g.height > 0) else {
            self.entries.insert(key, None);
            return Ok(None);
        };
        let kind = if g.color {
            AtlasKind::Color
        } else {
            AtlasKind::Mask
        };
        let (w, h) = (g.width as i32 + PAD, g.height as i32 + PAD);
        if w > self.max_size as i32 || h > self.max_size as i32 {
            tracing::warn!(
                width = g.width,
                height = g.height,
                "glyph larger than the atlas; skipped"
            );
            self.entries.insert(key, None);
            return Ok(None);
        }
        let page = &mut self.pages[kind as usize];
        let alloc = page.alloc.allocate(size2(w, h)).ok_or(AtlasFull(kind))?;
        let (x, y) = (alloc.rectangle.min.x as u32, alloc.rectangle.min.y as u32);
        let entry = AtlasEntry {
            kind,
            x,
            y,
            w: g.width,
            h: g.height,
            left: g.left,
            top: g.top,
        };
        self.uploads.push(Upload {
            kind,
            x,
            y,
            w: g.width,
            h: g.height,
            data: g.data,
        });
        self.entries.insert(key, Some(entry));
        Ok(Some(entry))
    }

    /// Forget every glyph (both pages). Bumps the generation.
    pub fn reset(&mut self) {
        for page in &mut self.pages {
            page.alloc.clear();
        }
        self.entries.clear();
        self.uploads.clear();
        self.generation += 1;
    }

    /// Double the page of `kind` (clears everything). Returns false at the
    /// size limit.
    pub fn grow(&mut self, kind: AtlasKind) -> bool {
        let size = self.pages[kind as usize].size;
        if size >= self.max_size {
            return false;
        }
        self.pages[kind as usize] = Page::new((size * 2).min(self.max_size));
        self.reset();
        true
    }

    pub fn take_uploads(&mut self) -> Vec<Upload> {
        std::mem::take(&mut self.uploads)
    }

    pub fn size(&self, kind: AtlasKind) -> u32 {
        self.pages[kind as usize].size
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn format_of(kind: AtlasKind) -> wgpu::TextureFormat {
    match kind {
        AtlasKind::Mask => wgpu::TextureFormat::R8Unorm,
        AtlasKind::Color => wgpu::TextureFormat::Rgba8Unorm,
    }
}

fn bytes_per_pixel(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Mask => 1,
        AtlasKind::Color => 4,
    }
}

fn create_texture(
    device: &wgpu::Device,
    kind: AtlasKind,
    size: u32,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(match kind {
            AtlasKind::Mask => "glyph-atlas-mask",
            AtlasKind::Color => "glyph-atlas-color",
        }),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: format_of(kind),
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}

/// GPU-backed atlas.
pub struct GlyphAtlas<K> {
    pub core: AtlasCore<K>,
    textures: [(wgpu::Texture, wgpu::TextureView, u32); 2],
    /// Bumped when textures are recreated (bind groups must be rebuilt).
    version: u64,
}

impl<K: Copy + Eq + Hash> GlyphAtlas<K> {
    pub const INITIAL_SIZE: u32 = 1024;

    pub fn new(device: &wgpu::Device) -> Self {
        let max = device.limits().max_texture_dimension_2d.min(8192);
        let core = AtlasCore::new(Self::INITIAL_SIZE, max);
        let mk = |kind| {
            let size = core.size(kind);
            let (t, v) = create_texture(device, kind, size);
            (t, v, size)
        };
        let textures = [mk(AtlasKind::Mask), mk(AtlasKind::Color)];
        Self {
            core,
            textures,
            version: 0,
        }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn view(&self, kind: AtlasKind) -> &wgpu::TextureView {
        &self.textures[kind as usize].1
    }

    /// Recreate textures whose page size changed and upload pending glyphs.
    pub fn flush(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        for kind in [AtlasKind::Mask, AtlasKind::Color] {
            let size = self.core.size(kind);
            if self.textures[kind as usize].2 != size {
                let (t, v) = create_texture(device, kind, size);
                self.textures[kind as usize] = (t, v, size);
                self.version += 1;
            }
        }
        for up in self.core.take_uploads() {
            let bpp = bytes_per_pixel(up.kind);
            debug_assert_eq!(up.data.len() as u32, up.w * up.h * bpp);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.textures[up.kind as usize].0,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: up.x,
                        y: up.y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &up.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(up.w * bpp),
                    rows_per_image: Some(up.h),
                },
                wgpu::Extent3d {
                    width: up.w,
                    height: up.h,
                    depth_or_array_layers: 1,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph(w: u32, h: u32, color: bool) -> RasterGlyph {
        let bpp = if color { 4 } else { 1 };
        RasterGlyph {
            left: 1,
            top: h as i32,
            width: w,
            height: h,
            color,
            data: vec![255; (w * h * bpp) as usize],
        }
    }

    #[test]
    fn insert_caches_and_records_upload() {
        let mut a: AtlasCore<u32> = AtlasCore::new(64, 128);
        assert_eq!(a.get(&1), None);
        let e = a.insert(1, Some(glyph(10, 12, false))).unwrap().unwrap();
        assert_eq!(
            (e.kind, e.w, e.h, e.left, e.top),
            (AtlasKind::Mask, 10, 12, 1, 12)
        );
        assert_eq!(a.get(&1), Some(Some(e)));
        let ups = a.take_uploads();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].data.len(), 120);
        assert!(a.take_uploads().is_empty());
    }

    #[test]
    fn empty_glyphs_are_cached_as_none() {
        let mut a: AtlasCore<u32> = AtlasCore::new(64, 128);
        assert_eq!(a.insert(7, None).unwrap(), None);
        assert_eq!(a.insert(8, Some(glyph(0, 0, false))).unwrap(), None);
        assert_eq!(a.get(&7), Some(None));
        assert!(a.take_uploads().is_empty());
    }

    #[test]
    fn color_glyphs_use_the_color_page() {
        let mut a: AtlasCore<u32> = AtlasCore::new(64, 128);
        let e = a.insert(1, Some(glyph(20, 20, true))).unwrap().unwrap();
        assert_eq!(e.kind, AtlasKind::Color);
        assert_eq!(a.take_uploads()[0].data.len(), 20 * 20 * 4);
    }

    #[test]
    fn full_page_reports_and_reset_recovers() {
        let mut a: AtlasCore<u32> = AtlasCore::new(64, 64);
        let mut inserted = 0;
        let full = loop {
            match a.insert(inserted, Some(glyph(15, 15, false))) {
                Ok(_) => inserted += 1,
                Err(e) => break e,
            }
            assert!(inserted < 100, "a 64² page cannot hold 100 16² glyphs");
        };
        assert_eq!(full, AtlasFull(AtlasKind::Mask));
        assert!(inserted >= 9);
        let gen = a.generation();
        a.reset();
        assert_eq!(a.generation(), gen + 1);
        assert!(a.is_empty());
        assert!(a.insert(1000, Some(glyph(15, 15, false))).is_ok());
    }

    #[test]
    fn grow_doubles_until_the_limit() {
        let mut a: AtlasCore<u32> = AtlasCore::new(64, 256);
        a.insert(1, Some(glyph(8, 8, false))).unwrap();
        assert!(a.grow(AtlasKind::Mask));
        assert_eq!(a.size(AtlasKind::Mask), 128);
        assert!(a.is_empty(), "grow invalidates entries");
        assert!(a.grow(AtlasKind::Mask));
        assert_eq!(a.size(AtlasKind::Mask), 256);
        assert!(!a.grow(AtlasKind::Mask));
        assert_eq!(a.size(AtlasKind::Color), 64);
    }

    #[test]
    fn oversized_glyph_is_skipped_not_looping() {
        let mut a: AtlasCore<u32> = AtlasCore::new(32, 32);
        assert_eq!(a.insert(1, Some(glyph(40, 10, false))).unwrap(), None);
        assert_eq!(a.get(&1), Some(None));
    }
}
