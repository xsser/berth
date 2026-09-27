//! Terminal grid renderer: wgpu instanced quads, cosmic-text shaping with
//! per-cell placement, swash rasterization and an etagere glyph atlas.

pub mod atlas;
pub mod grid;
pub mod metrics;
pub mod sprites;
pub mod text;

pub use grid::{
    clip_rect, fill_quad, outline_quad, FrameInput, GridLayout, GridRenderer, PaneInput,
    PrepareStats, QuadInstance,
};
pub use metrics::CellMetrics;
