//! Cell geometry derived from the primary font (DESIGN §8.1).
//!
//! All results are whole pixels so every cell, glyph quad and decoration
//! lands on the pixel grid (no blurry seams between cells).

/// Raw metrics of the primary face, in font units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceMetrics {
    pub units_per_em: f32,
    /// Above the baseline (positive).
    pub ascent: f32,
    /// Below the baseline (positive).
    pub descent: f32,
    pub line_gap: f32,
    /// Advance of the reference glyph ('0'), i.e. the monospace pitch.
    pub advance: f32,
    /// Top of the underline relative to the baseline (negative = below).
    pub underline_offset: f32,
    pub underline_thickness: f32,
    /// Top of the strikeout stroke above the baseline (0 if unknown).
    pub strikeout_offset: f32,
    pub x_height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// Font size in physical pixels (points × scale factor).
    pub font_px: f32,
    pub cell_w: u32,
    pub cell_h: u32,
    /// Distance from the cell top to the baseline.
    pub baseline: u32,
    /// Distance from the cell top to the top of the underline.
    pub underline_top: u32,
    pub underline_thickness: u32,
    pub strikeout_top: u32,
    pub strikeout_thickness: u32,
    /// Width of beam / height of underline cursors.
    pub cursor_thickness: u32,
    /// Stroke width for procedurally drawn box-drawing lines.
    pub box_thickness: u32,
}

impl CellMetrics {
    pub fn compute(face: &FaceMetrics, font_px: f32) -> Self {
        let s = font_px / face.units_per_em.max(1.0);
        let ascent = face.ascent * s;
        let descent = face.descent * s;
        let gap = face.line_gap.max(0.0) * s;
        let cell_w = (face.advance * s).round().max(1.0) as u32;
        let cell_h = (ascent + descent + gap).round().max(2.0) as u32;
        // Split any extra height evenly above and below the glyph box.
        let baseline = ((cell_h as f32 - (ascent + descent)) / 2.0 + ascent)
            .round()
            .clamp(1.0, cell_h as f32 - 1.0) as u32;

        let underline_thickness = (face.underline_thickness * s).round().max(1.0) as u32;
        let underline_top = (baseline as f32 - face.underline_offset * s)
            .round()
            .clamp(baseline as f32, (cell_h - underline_thickness) as f32)
            as u32;

        let strikeout_thickness = underline_thickness;
        let strike_center = if face.strikeout_offset > 0.0 {
            face.strikeout_offset * s - strikeout_thickness as f32 / 2.0
        } else if face.x_height > 0.0 {
            face.x_height * s / 2.0
        } else {
            ascent * 0.3
        };
        let strikeout_top = (baseline as f32 - strike_center - strikeout_thickness as f32 / 2.0)
            .round()
            .clamp(0.0, (baseline - 1) as f32) as u32;

        let cursor_thickness = (font_px / 13.0).round().clamp(1.0, 4.0) as u32;
        let box_thickness = (face.underline_thickness * s).ceil().max(1.0) as u32;
        Self {
            font_px,
            cell_w,
            cell_h,
            baseline,
            underline_top,
            underline_thickness,
            strikeout_top,
            strikeout_thickness,
            cursor_thickness,
            box_thickness,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Metrics shaped like SF Mono (upem 2048).
    fn sf_mono_like() -> FaceMetrics {
        FaceMetrics {
            units_per_em: 2048.0,
            ascent: 1950.0,
            descent: 494.0,
            line_gap: 0.0,
            advance: 1266.0,
            underline_offset: -150.0,
            underline_thickness: 100.0,
            strikeout_offset: 700.0,
            x_height: 1100.0,
        }
    }

    #[test]
    fn retina_13pt_cell_is_integral_and_consistent() {
        let m = CellMetrics::compute(&sf_mono_like(), 26.0);
        assert_eq!(m.cell_w, 16); // 1266/2048*26 = 16.07
        assert_eq!(m.cell_h, 31); // (1950+494)/2048*26 = 31.03
        assert!(m.baseline > 0 && m.baseline < m.cell_h);
        assert!(m.underline_top >= m.baseline, "underline below baseline");
        assert!(
            m.underline_top + m.underline_thickness <= m.cell_h,
            "underline inside cell"
        );
        assert!(m.strikeout_top < m.baseline, "strikeout above baseline");
        assert!(m.box_thickness >= 1 && m.cursor_thickness >= 1);
    }

    #[test]
    fn line_gap_is_split_around_the_glyph_box() {
        let mut face = sf_mono_like();
        face.line_gap = 400.0;
        let tight = CellMetrics::compute(&sf_mono_like(), 26.0);
        let loose = CellMetrics::compute(&face, 26.0);
        assert!(loose.cell_h > tight.cell_h);
        let extra = loose.cell_h - tight.cell_h;
        let shift = loose.baseline - tight.baseline;
        assert!(shift.abs_diff(extra / 2) <= 1);
    }

    #[test]
    fn missing_decoration_metrics_fall_back_sanely() {
        let face = FaceMetrics {
            underline_offset: 0.0,
            underline_thickness: 0.0,
            strikeout_offset: 0.0,
            x_height: 0.0,
            ..sf_mono_like()
        };
        let m = CellMetrics::compute(&face, 13.0);
        assert_eq!(m.underline_thickness, 1);
        assert!(m.strikeout_top < m.baseline);
        assert!(m.underline_top + m.underline_thickness <= m.cell_h);
    }
}
