//! Terminal grid renderer: wgpu instanced quads, cosmic-text shaping with
//! per-cell placement, swash rasterization and an etagere glyph atlas.

pub mod atlas;
pub mod grid;
pub mod metrics;
pub mod sprites;
pub mod text;

pub use grid::{FrameInput, GridLayout, GridRenderer, PrepareStats};
pub use metrics::CellMetrics;
