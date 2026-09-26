//! Frame-time statistics for the M0 acceptance (≤ 8 ms/frame at 120×40).
//!
//! A frame is split into the CPU work we control (grid build + sidebar +
//! uploads + encode/submit), the time blocked waiting for a drawable (vsync
//! back-pressure, not work) and — in `--bench` mode, where every frame is
//! synchronised with the GPU — the GPU execution time. The first frame is
//! reported separately: it shapes every line and rasterizes every glyph.

use std::fmt;
use std::time::{Duration, Instant};

/// A series of millisecond samples.
#[derive(Clone, Debug, Default)]
pub struct Series {
    samples: Vec<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub avg: f64,
    pub p50: f64,
    pub p99: f64,
    pub max: f64,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "avg {:6.3} ms  p50 {:6.3}  p99 {:6.3}  max {:6.3}  (n={})",
            self.avg, self.p50, self.p99, self.max, self.n
        )
    }
}

impl Series {
    pub fn push(&mut self, d: Duration) {
        self.samples.push(d.as_secs_f64() * 1000.0);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn summary(&self) -> Option<Summary> {
        if self.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(f64::total_cmp);
        let pct = |p: f64| {
            let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };
        Some(Summary {
            n: sorted.len(),
            avg: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50: pct(50.0),
            p99: pct(99.0),
            max: sorted[sorted.len() - 1],
        })
    }
}

/// Timings of one rendered frame.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    /// Grid instance build + egui run/tessellate + buffer/texture uploads.
    pub prepare: Duration,
    /// Blocked in `get_current_texture` (vsync back-pressure).
    pub acquire: Duration,
    /// Command encoding, submit and present.
    pub encode: Duration,
    /// Submit → GPU completion (measured in `--bench` and offscreen frames).
    pub gpu: Option<Duration>,
    /// Rendered offscreen because the window was occluded (no present).
    pub offscreen: bool,
}

impl FrameTiming {
    pub fn cpu(&self) -> Duration {
        self.prepare + self.encode
    }

    /// CPU + GPU work for the frame when both were measured.
    pub fn total(&self) -> Duration {
        self.cpu() + self.gpu.unwrap_or_default()
    }
}

/// Aggregates frame timings over a measurement window.
#[derive(Debug, Default)]
pub struct FrameStats {
    first: Option<FrameTiming>,
    prepare: Series,
    cpu: Series,
    gpu: Series,
    total: Series,
    acquire: Series,
    interval: Series,
    last_start: Option<Instant>,
    window_start: Option<Instant>,
    window_end: Option<Instant>,
    offscreen_frames: usize,
}

impl FrameStats {
    /// Record a frame that started at `start`. The very first frame is kept
    /// apart as the cold-start sample.
    pub fn record(&mut self, start: Instant, t: FrameTiming) {
        if self.first.is_none() {
            self.first = Some(t);
            self.last_start = Some(start);
            return;
        }
        if self.window_start.is_none() {
            self.window_start = Some(start);
        }
        if let Some(prev) = self.last_start {
            self.interval.push(start - prev);
        }
        self.last_start = Some(start);
        self.window_end = Some(Instant::now());
        self.offscreen_frames += usize::from(t.offscreen);
        self.prepare.push(t.prepare);
        self.cpu.push(t.cpu());
        self.acquire.push(t.acquire);
        if let Some(gpu) = t.gpu {
            self.gpu.push(gpu);
        }
        self.total.push(t.total());
    }

    pub fn frames(&self) -> usize {
        self.cpu.len()
    }

    /// Worst total frame time (steady state), for pass/fail.
    pub fn total_summary(&self) -> Option<Summary> {
        self.total.summary()
    }

    /// Multi-line human report for stderr.
    pub fn report(&self, title: &str, target_ms: f64) -> String {
        let mut out = String::new();
        let elapsed = match (self.window_start, self.window_end) {
            (Some(a), Some(b)) => (b - a).as_secs_f64(),
            _ => 0.0,
        };
        let fps = if elapsed > 0.0 {
            self.frames() as f64 / elapsed
        } else {
            0.0
        };
        out.push_str(&format!(
            "[stats] {title}: {} frames in {elapsed:.2}s ({fps:.1} fps)\n",
            self.frames()
        ));
        if self.offscreen_frames > 0 {
            out.push_str(&format!(
                "[stats]   render target: {} of {} frames offscreen (window occluded; no present/vsync)\n",
                self.offscreen_frames,
                self.frames()
            ));
        } else {
            out.push_str("[stats]   render target: window surface (presented)\n");
        }
        if let Some(f) = self.first {
            out.push_str(&format!(
                "[stats]   first frame of phase (excluded from stats): cpu {:.2} ms{}\n",
                f.cpu().as_secs_f64() * 1000.0,
                f.gpu
                    .map(|g| format!(", gpu {:.2} ms", g.as_secs_f64() * 1000.0))
                    .unwrap_or_default()
            ));
        }
        let mut line = |name: &str, s: &Series| {
            if let Some(sum) = s.summary() {
                out.push_str(&format!("[stats]   {name:<34} {sum}\n"));
            }
        };
        line("prepare (grid + sidebar + upload)", &self.prepare);
        line("cpu (prepare + encode/submit)", &self.cpu);
        line("gpu (submit → done)", &self.gpu);
        line("frame total (cpu + gpu)", &self.total);
        line("acquire wait (vsync, not work)", &self.acquire);
        line("frame interval (wall clock)", &self.interval);
        if let Some(sum) = self.total_summary() {
            let verdict = if sum.p99 <= target_ms { "PASS" } else { "FAIL" };
            out.push_str(&format!(
                "[stats]   target ≤ {target_ms} ms/frame: {verdict} (avg {:.3} ms, p99 {:.3} ms, max {:.3} ms)\n",
                sum.avg, sum.p99, sum.max
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_percentiles() {
        let mut s = Series::default();
        for ms in 1..=100 {
            s.push(Duration::from_millis(ms));
        }
        let sum = s.summary().unwrap();
        assert_eq!(sum.n, 100);
        assert!((sum.avg - 50.5).abs() < 1e-9);
        assert!((sum.p50 - 51.0).abs() < 1e-9 || (sum.p50 - 50.0).abs() < 1e-9);
        assert!((sum.p99 - 99.0).abs() < 1.0 + 1e-9);
        assert!((sum.max - 100.0).abs() < 1e-9);
        assert!(Series::default().summary().is_none());
    }

    #[test]
    fn first_frame_is_kept_apart() {
        let mut st = FrameStats::default();
        let t0 = Instant::now();
        st.record(
            t0,
            FrameTiming {
                prepare: Duration::from_millis(80),
                ..Default::default()
            },
        );
        assert_eq!(st.frames(), 0);
        assert!(st.first.is_some());
        for i in 1..=10 {
            st.record(
                t0 + Duration::from_millis(8 * i),
                FrameTiming {
                    prepare: Duration::from_millis(1),
                    encode: Duration::from_millis(1),
                    gpu: Some(Duration::from_millis(1)),
                    ..Default::default()
                },
            );
        }
        assert_eq!(st.frames(), 10);
        let total = st.total_summary().unwrap();
        assert!((total.max - 3.0).abs() < 1e-9);
        let report = st.report("test", 8.0);
        assert!(report.contains("PASS"), "{report}");
        assert!(report.contains("first frame"), "{report}");
    }
}
