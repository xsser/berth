//! Procedurally drawn cell sprites: box drawing (U+2500–U+257F), block
//! elements (U+2580–U+259F) and Powerline separators (U+E0B0–U+E0B3).
//!
//! Font glyphs for these characters rarely span the full cell height, which
//! leaves gaps between rows (`│` stacks) and depends on the fallback font
//! having them at all. Like Ghostty/kitty we draw them into an alpha mask of
//! exactly one cell so lines join seamlessly across cells and rows.

/// Whether `c` is drawn procedurally instead of from a font.
pub fn is_sprite(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x259F | 0xE0B0..=0xE0B3)
}

struct Canvas {
    w: i32,
    h: i32,
    px: Vec<u8>,
}

impl Canvas {
    fn new(w: u32, h: u32) -> Self {
        Self {
            w: w as i32,
            h: h as i32,
            px: vec![0; (w * h) as usize],
        }
    }

    /// Fill `[x0, x1) × [y0, y1)` (clamped) with `alpha` (max-blend).
    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, alpha: u8) {
        let (x0, x1) = (x0.clamp(0, self.w), x1.clamp(0, self.w));
        let (y0, y1) = (y0.clamp(0, self.h), y1.clamp(0, self.h));
        for y in y0..y1 {
            let row = (y * self.w) as usize;
            for x in x0..x1 {
                let p = &mut self.px[row + x as usize];
                *p = (*p).max(alpha);
            }
        }
    }

    /// Anti-aliased fill from a point predicate, 4×4 supersampled.
    fn shape(&mut self, inside: impl Fn(f32, f32) -> bool) {
        for y in 0..self.h {
            for x in 0..self.w {
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let fx = x as f32 + (sx as f32 + 0.5) / 4.0;
                        let fy = y as f32 + (sy as f32 + 0.5) / 4.0;
                        hits += inside(fx, fy) as u32;
                    }
                }
                if hits > 0 {
                    let a = ((hits * 255 + 8) / 16) as u8;
                    let p = &mut self.px[(y * self.w + x) as usize];
                    *p = (*p).max(a);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    No,
    Light,
    Heavy,
}

use Arm::{Heavy as H, Light as L, No as N};

/// Arms (up, right, down, left) for U+2500..=U+254B and U+2574..=U+257F.
fn arms(c: u32) -> Option<[Arm; 4]> {
    Some(match c {
        0x2500 => [N, L, N, L],
        0x2501 => [N, H, N, H],
        0x2502 => [L, N, L, N],
        0x2503 => [H, N, H, N],
        0x250C => [N, L, L, N],
        0x250D => [N, H, L, N],
        0x250E => [N, L, H, N],
        0x250F => [N, H, H, N],
        0x2510 => [N, N, L, L],
        0x2511 => [N, N, L, H],
        0x2512 => [N, N, H, L],
        0x2513 => [N, N, H, H],
        0x2514 => [L, L, N, N],
        0x2515 => [L, H, N, N],
        0x2516 => [H, L, N, N],
        0x2517 => [H, H, N, N],
        0x2518 => [L, N, N, L],
        0x2519 => [L, N, N, H],
        0x251A => [H, N, N, L],
        0x251B => [H, N, N, H],
        0x251C => [L, L, L, N],
        0x251D => [L, H, L, N],
        0x251E => [H, L, L, N],
        0x251F => [L, L, H, N],
        0x2520 => [H, L, H, N],
        0x2521 => [H, H, L, N],
        0x2522 => [L, H, H, N],
        0x2523 => [H, H, H, N],
        0x2524 => [L, N, L, L],
        0x2525 => [L, N, L, H],
        0x2526 => [H, N, L, L],
        0x2527 => [L, N, H, L],
        0x2528 => [H, N, H, L],
        0x2529 => [H, N, L, H],
        0x252A => [L, N, H, H],
        0x252B => [H, N, H, H],
        0x252C => [N, L, L, L],
        0x252D => [N, L, L, H],
        0x252E => [N, H, L, L],
        0x252F => [N, H, L, H],
        0x2530 => [N, L, H, L],
        0x2531 => [N, L, H, H],
        0x2532 => [N, H, H, L],
        0x2533 => [N, H, H, H],
        0x2534 => [L, L, N, L],
        0x2535 => [L, L, N, H],
        0x2536 => [L, H, N, L],
        0x2537 => [L, H, N, H],
        0x2538 => [H, L, N, L],
        0x2539 => [H, L, N, H],
        0x253A => [H, H, N, L],
        0x253B => [H, H, N, H],
        0x253C => [L, L, L, L],
        0x253D => [L, L, L, H],
        0x253E => [L, H, L, L],
        0x253F => [L, H, L, H],
        0x2540 => [H, L, L, L],
        0x2541 => [L, L, H, L],
        0x2542 => [H, L, H, L],
        0x2543 => [H, L, L, H],
        0x2544 => [H, H, L, L],
        0x2545 => [L, L, H, H],
        0x2546 => [L, H, H, L],
        0x2547 => [H, H, L, H],
        0x2548 => [L, H, H, H],
        0x2549 => [H, L, H, H],
        0x254A => [H, H, H, L],
        0x254B => [H, H, H, H],
        0x2574 => [N, N, N, L],
        0x2575 => [L, N, N, N],
        0x2576 => [N, L, N, N],
        0x2577 => [N, N, L, N],
        0x2578 => [N, N, N, H],
        0x2579 => [H, N, N, N],
        0x257A => [N, H, N, N],
        0x257B => [N, N, H, N],
        0x257C => [N, H, N, L],
        0x257D => [L, N, H, N],
        0x257E => [N, L, N, H],
        0x257F => [H, N, L, N],
        _ => return None,
    })
}

/// Symbolic coordinates for double-line segments.
#[derive(Clone, Copy)]
enum P {
    /// Cell edge (left/top).
    E0,
    /// Outer/first line of a double (center − gap).
    D1,
    /// Center.
    C,
    /// Second line of a double (center + gap).
    D2,
    /// Cell edge (right/bottom).
    E1,
}

/// A line segment: horizontal (`true`) at `pos` from `a` to `b`, or vertical.
type Seg = (bool, P, P, P);

fn double_segments(c: u32) -> Option<&'static [Seg]> {
    use P::*;
    const T: bool = true; // horizontal
    const F: bool = false; // vertical
    Some(match c {
        0x2550 => &[(T, D1, E0, E1), (T, D2, E0, E1)],
        0x2551 => &[(F, D1, E0, E1), (F, D2, E0, E1)],
        0x2552 => &[(T, D1, C, E1), (T, D2, C, E1), (F, C, D1, E1)],
        0x2553 => &[(T, C, D1, E1), (F, D1, C, E1), (F, D2, C, E1)],
        0x2554 => &[
            (T, D1, D1, E1),
            (T, D2, D2, E1),
            (F, D1, D1, E1),
            (F, D2, D2, E1),
        ],
        0x2555 => &[(T, D1, E0, C), (T, D2, E0, C), (F, C, D1, E1)],
        0x2556 => &[(T, C, E0, D2), (F, D1, C, E1), (F, D2, C, E1)],
        0x2557 => &[
            (T, D1, E0, D2),
            (T, D2, E0, D1),
            (F, D2, D1, E1),
            (F, D1, D2, E1),
        ],
        0x2558 => &[(T, D1, C, E1), (T, D2, C, E1), (F, C, E0, D2)],
        0x2559 => &[(T, C, D1, E1), (F, D1, E0, C), (F, D2, E0, C)],
        0x255A => &[
            (T, D2, D1, E1),
            (T, D1, D2, E1),
            (F, D1, E0, D2),
            (F, D2, E0, D1),
        ],
        0x255B => &[(T, D1, E0, C), (T, D2, E0, C), (F, C, E0, D2)],
        0x255C => &[(T, C, E0, D2), (F, D1, E0, C), (F, D2, E0, C)],
        0x255D => &[
            (T, D2, E0, D2),
            (T, D1, E0, D1),
            (F, D2, E0, D2),
            (F, D1, E0, D1),
        ],
        0x255E => &[(F, C, E0, E1), (T, D1, C, E1), (T, D2, C, E1)],
        0x255F => &[(F, D1, E0, E1), (F, D2, E0, E1), (T, C, D2, E1)],
        0x2560 => &[
            (F, D1, E0, E1),
            (F, D2, E0, D1),
            (F, D2, D2, E1),
            (T, D1, D2, E1),
            (T, D2, D2, E1),
        ],
        0x2561 => &[(F, C, E0, E1), (T, D1, E0, C), (T, D2, E0, C)],
        0x2562 => &[(F, D1, E0, E1), (F, D2, E0, E1), (T, C, E0, D1)],
        0x2563 => &[
            (F, D2, E0, E1),
            (F, D1, E0, D1),
            (F, D1, D2, E1),
            (T, D1, E0, D1),
            (T, D2, E0, D1),
        ],
        0x2564 => &[(T, D1, E0, E1), (T, D2, E0, E1), (F, C, D2, E1)],
        0x2565 => &[(T, C, E0, E1), (F, D1, C, E1), (F, D2, C, E1)],
        0x2566 => &[
            (T, D1, E0, E1),
            (T, D2, E0, D1),
            (T, D2, D2, E1),
            (F, D1, D2, E1),
            (F, D2, D2, E1),
        ],
        0x2567 => &[(T, D1, E0, E1), (T, D2, E0, E1), (F, C, E0, D1)],
        0x2568 => &[(T, C, E0, E1), (F, D1, E0, C), (F, D2, E0, C)],
        0x2569 => &[
            (T, D2, E0, E1),
            (T, D1, E0, D1),
            (T, D1, D2, E1),
            (F, D1, E0, D1),
            (F, D2, E0, D1),
        ],
        0x256A => &[(T, D1, E0, E1), (T, D2, E0, E1), (F, C, E0, E1)],
        0x256B => &[(F, D1, E0, E1), (F, D2, E0, E1), (T, C, E0, E1)],
        0x256C => &[
            (F, D1, E0, D1),
            (F, D1, D2, E1),
            (F, D2, E0, D1),
            (F, D2, D2, E1),
            (T, D1, E0, D1),
            (T, D1, D2, E1),
            (T, D2, E0, D1),
            (T, D2, D2, E1),
        ],
        _ => return None,
    })
}

/// Render `c` into a `w × h` alpha mask. `t` is the light stroke width.
pub fn render(c: char, w: u32, h: u32, t: u32) -> Option<Vec<u8>> {
    if !is_sprite(c) || w == 0 || h == 0 {
        return None;
    }
    let mut cv = Canvas::new(w, h);
    let (wi, hi) = (w as i32, h as i32);
    let t = t.max(1) as i32;
    let heavy = t * 2;
    let cx = wi / 2;
    let cy = hi / 2;
    // A stroke of thickness `k` centred on the cell centre occupies [c - k/2, c - k/2 + k).
    let lo = |center: i32, k: i32| center - k / 2;
    let code = c as u32;

    if let Some([up, right, down, left]) = arms(code) {
        let th = |a: Arm| match a {
            Arm::No => 0,
            Arm::Light => t,
            Arm::Heavy => heavy,
        };
        let vert = th(up).max(th(down));
        let horiz = th(left).max(th(right));
        let join = |perp: i32, own: i32| if perp > 0 { perp } else { own };
        if right != N {
            let k = th(right);
            let j = join(vert, k);
            cv.rect(lo(cx, j), lo(cy, k), wi, lo(cy, k) + k, 255);
        }
        if left != N {
            let k = th(left);
            let j = join(vert, k);
            cv.rect(0, lo(cy, k), lo(cx, j) + j, lo(cy, k) + k, 255);
        }
        if down != N {
            let k = th(down);
            let j = join(horiz, k);
            cv.rect(lo(cx, k), lo(cy, j), lo(cx, k) + k, hi, 255);
        }
        if up != N {
            let k = th(up);
            let j = join(horiz, k);
            cv.rect(lo(cx, k), 0, lo(cx, k) + k, lo(cy, j) + j, 255);
        }
        return Some(cv.px);
    }

    if let Some(segs) = double_segments(code) {
        let gap = t; // distance between the centre line and each double line
        let coord = |p: P, center: i32, edge: i32| match p {
            P::E0 => 0,
            P::D1 => center - gap,
            P::C => center,
            P::D2 => center + gap,
            P::E1 => edge,
        };
        let interior = |p: P| !matches!(p, P::E0 | P::E1);
        for &(horizontal, pos, a, b) in segs {
            if horizontal {
                let y = coord(pos, cy, hi);
                let mut x0 = coord(a, cx, wi);
                let mut x1 = coord(b, cx, wi);
                if interior(a) {
                    x0 = lo(x0, t);
                }
                if interior(b) {
                    x1 = lo(x1, t) + t;
                }
                cv.rect(x0, lo(y, t), x1, lo(y, t) + t, 255);
            } else {
                let x = coord(pos, cx, wi);
                let mut y0 = coord(a, cy, hi);
                let mut y1 = coord(b, cy, hi);
                if interior(a) {
                    y0 = lo(y0, t);
                }
                if interior(b) {
                    y1 = lo(y1, t) + t;
                }
                cv.rect(lo(x, t), y0, lo(x, t) + t, y1, 255);
            }
        }
        return Some(cv.px);
    }

    match code {
        // Dashed lines: (dashes, heavy, horizontal).
        0x2504..=0x250B | 0x254C..=0x254F => {
            let (n, is_heavy, horizontal) = match code {
                0x2504 => (3, false, true),
                0x2505 => (3, true, true),
                0x2506 => (3, false, false),
                0x2507 => (3, true, false),
                0x2508 => (4, false, true),
                0x2509 => (4, true, true),
                0x250A => (4, false, false),
                0x250B => (4, true, false),
                0x254C => (2, false, true),
                0x254D => (2, true, true),
                0x254E => (2, false, false),
                _ => (2, true, false),
            };
            let k = if is_heavy { heavy } else { t };
            let len = if horizontal { wi } else { hi };
            for i in 0..n {
                let s0 = len * i / n;
                let s1 = len * (i + 1) / n;
                let gap = ((s1 - s0) as f32 * 0.35).round().max(1.0) as i32;
                let (a, b) = (s0 + gap / 2, s1 - (gap - gap / 2));
                if horizontal {
                    cv.rect(a, lo(cy, k), b, lo(cy, k) + k, 255);
                } else {
                    cv.rect(lo(cx, k), a, lo(cx, k) + k, b, 255);
                }
            }
        }
        // Rounded corners.
        0x256D..=0x2570 => {
            let lcx = lo(cx, t) as f32 + t as f32 / 2.0;
            let lcy = lo(cy, t) as f32 + t as f32 / 2.0;
            let (wf, hf) = (wi as f32, hi as f32);
            // (towards right?, towards down?)
            let (right, down) = match code {
                0x256D => (true, true),
                0x256E => (false, true),
                0x256F => (false, false),
                _ => (true, false),
            };
            let rx = if right { wf - lcx } else { lcx };
            let ry = if down { hf - lcy } else { lcy };
            let r = rx.min(ry);
            let ccx = if right { lcx + r } else { lcx - r };
            let ccy = if down { lcy + r } else { lcy - r };
            let half = t as f32 / 2.0;
            cv.shape(|x, y| {
                let in_quadrant = (if right { x <= ccx } else { x >= ccx })
                    && (if down { y <= ccy } else { y >= ccy });
                if !in_quadrant {
                    return false;
                }
                let d = ((x - ccx).powi(2) + (y - ccy).powi(2)).sqrt();
                (d - r).abs() <= half
            });
            // Straight continuations to the cell edges.
            let (vx0, vx1) = (lo(cx, t), lo(cx, t) + t);
            let (hy0, hy1) = (lo(cy, t), lo(cy, t) + t);
            let arc_y = ccy.round() as i32;
            let arc_x = ccx.round() as i32;
            if down {
                cv.rect(vx0, arc_y, vx1, hi, 255);
            } else {
                cv.rect(vx0, 0, vx1, arc_y, 255);
            }
            if right {
                cv.rect(arc_x, hy0, wi, hy1, 255);
            } else {
                cv.rect(0, hy0, arc_x, hy1, 255);
            }
        }
        // Diagonals.
        0x2571..=0x2573 => {
            let (wf, hf) = (wi as f32, hi as f32);
            let len = (wf * wf + hf * hf).sqrt();
            let half = t as f32 / 2.0 + 0.25;
            let rising = |x: f32, y: f32| ((hf * x + wf * y - wf * hf) / len).abs() <= half; // (0,h)-(w,0)
            let falling = |x: f32, y: f32| ((hf * x - wf * y) / len).abs() <= half; // (0,0)-(w,h)
            match code {
                0x2571 => cv.shape(rising),
                0x2572 => cv.shape(falling),
                _ => cv.shape(|x, y| rising(x, y) || falling(x, y)),
            }
        }
        // Block elements.
        0x2580 => cv.rect(0, 0, wi, hi / 2, 255),
        0x2581..=0x2588 => {
            let n = (code - 0x2580) as i32; // eighths from the bottom
            cv.rect(0, hi - (hi * n + 4) / 8, wi, hi, 255);
        }
        0x2589..=0x258F => {
            let n = (0x2590 - code) as i32; // 7..1 eighths from the left
            cv.rect(0, 0, (wi * n + 4) / 8, hi, 255);
        }
        0x2590 => cv.rect(wi / 2, 0, wi, hi, 255),
        0x2591..=0x2593 => {
            let a = [64u8, 128, 191][(code - 0x2591) as usize];
            cv.rect(0, 0, wi, hi, a);
        }
        0x2594 => cv.rect(0, 0, wi, (hi + 4) / 8, 255),
        0x2595 => cv.rect(wi - (wi + 4) / 8, 0, wi, hi, 255),
        0x2596..=0x259F => {
            // Quadrants: bit 0 UL, 1 UR, 2 LL, 3 LR.
            let q = match code {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                _ => 0b1110,
            };
            let (mx, my) = (wi / 2, hi / 2);
            if q & 1 != 0 {
                cv.rect(0, 0, mx, my, 255);
            }
            if q & 2 != 0 {
                cv.rect(mx, 0, wi, my, 255);
            }
            if q & 4 != 0 {
                cv.rect(0, my, mx, hi, 255);
            }
            if q & 8 != 0 {
                cv.rect(mx, my, wi, hi, 255);
            }
        }
        // Powerline separators.
        0xE0B0..=0xE0B3 => {
            let (wf, hf) = (wi as f32, hi as f32);
            let pointing_right = code <= 0xE0B1;
            let solid = code == 0xE0B0 || code == 0xE0B2;
            if solid {
                cv.shape(|x, y| {
                    let reach = wf * (1.0 - (2.0 * y / hf - 1.0).abs());
                    if pointing_right {
                        x <= reach
                    } else {
                        x >= wf - reach
                    }
                });
            } else {
                let half = t as f32 / 2.0 + 0.25;
                let seg_dist = |px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32| {
                    let (dx, dy) = (bx - ax, by - ay);
                    let u =
                        (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                    ((px - ax - u * dx).powi(2) + (py - ay - u * dy).powi(2)).sqrt()
                };
                let (tip_x, back_x) = if pointing_right {
                    (wf - half, half)
                } else {
                    (half, wf - half)
                };
                cv.shape(|x, y| {
                    seg_dist(x, y, back_x, 0.0, tip_x, hf / 2.0) <= half
                        || seg_dist(x, y, tip_x, hf / 2.0, back_x, hf) <= half
                });
            }
        }
        _ => return None,
    }
    Some(cv.px)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 16;
    const H: u32 = 32;

    fn at(mask: &[u8], x: u32, y: u32) -> u8 {
        mask[(y * W + x) as usize]
    }

    #[test]
    fn horizontal_and_vertical_lines_reach_every_edge() {
        let h = render('─', W, H, 2).unwrap();
        assert_eq!(at(&h, 0, H / 2), 255);
        assert_eq!(at(&h, W - 1, H / 2), 255);
        assert_eq!(at(&h, W / 2, 0), 0);
        let v = render('│', W, H, 2).unwrap();
        assert_eq!(at(&v, W / 2, 0), 255);
        assert_eq!(at(&v, W / 2, H - 1), 255);
        assert_eq!(at(&v, 0, H / 2), 0);
    }

    #[test]
    fn corner_only_has_its_two_arms() {
        let m = render('┌', W, H, 2).unwrap();
        assert_eq!(at(&m, W - 1, H / 2), 255, "right arm");
        assert_eq!(at(&m, W / 2, H - 1), 255, "down arm");
        assert_eq!(at(&m, 0, H / 2), 0, "no left arm");
        assert_eq!(at(&m, W / 2, 0), 0, "no up arm");
        assert_eq!(at(&m, W / 2, H / 2), 255, "joint filled");
    }

    #[test]
    fn heavy_is_thicker_than_light() {
        let count = |m: &[u8]| (0..H).filter(|&y| at(m, W - 1, y) == 255).count();
        let light = render('─', W, H, 2).unwrap();
        let heavy = render('━', W, H, 2).unwrap();
        assert_eq!(count(&light), 2);
        assert_eq!(count(&heavy), 4);
    }

    #[test]
    fn double_lines_have_a_gap() {
        let m = render('═', W, H, 2).unwrap();
        let col: Vec<u8> = (0..H).map(|y| at(&m, 0, y)).collect();
        let runs = col.windows(2).filter(|w| w[0] == 0 && w[1] == 255).count();
        assert_eq!(runs, 2, "two separate strokes: {col:?}");
        let corner = render('╔', W, H, 2).unwrap();
        assert_eq!(at(&corner, 0, H / 2), 0);
        assert!(at(&corner, W - 1, H / 2 - 2) == 255 || at(&corner, W - 1, H / 2 - 1) == 255);
    }

    #[test]
    fn rounded_corner_connects_to_edges() {
        let m = render('╭', W, H, 2).unwrap();
        assert_eq!(at(&m, W / 2, H - 1), 255, "continues down");
        assert!(at(&m, W - 1, H / 2) > 0, "reaches right edge");
        assert_eq!(at(&m, 0, 0), 0);
        assert_eq!(at(&m, 1, H / 2), 0, "nothing to the left");
    }

    #[test]
    fn blocks_and_shades() {
        let full = render('█', W, H, 2).unwrap();
        assert!(full.iter().all(|&a| a == 255));
        let upper = render('▀', W, H, 2).unwrap();
        assert_eq!(at(&upper, 3, 0), 255);
        assert_eq!(at(&upper, 3, H - 1), 0);
        let shade = render('▒', W, H, 2).unwrap();
        assert!(shade.iter().all(|&a| a == 128));
        let quadrant = render('▚', W, H, 2).unwrap();
        assert_eq!(at(&quadrant, 0, 0), 255);
        assert_eq!(at(&quadrant, W - 1, 0), 0);
        assert_eq!(at(&quadrant, W - 1, H - 1), 255);
    }

    #[test]
    fn powerline_triangle_shape() {
        let m = render('\u{e0b0}', W, H, 2).unwrap();
        assert_eq!(at(&m, 0, 1), 255, "left column covered");
        assert_eq!(at(&m, W - 1, 0), 0, "tip only at mid-height");
        assert!(at(&m, W - 1, H / 2) > 0);
        assert!(render('\u{e0b1}', W, H, 2).unwrap().iter().any(|&a| a > 0));
    }

    #[test]
    fn non_sprites_are_rejected() {
        assert!(!is_sprite('a'));
        assert!(render('a', W, H, 2).is_none());
        assert!(is_sprite('╳') && is_sprite('▟') && is_sprite('\u{e0b3}'));
    }
}
