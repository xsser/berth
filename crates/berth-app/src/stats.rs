//! Frame-time statistics for the M0 acceptance (≤ 8 ms/frame at 120×40)
//! and the M4 split one (4 panes of 120×40: p99 < 4 ms/frame, DESIGN
//! §17.3).
//!
//! A frame is split into the CPU work we control (grid build + sidebar +
//! uploads + encode/submit), the time blocked waiting for a drawable (vsync
//! back-pressure, not work) and — in `--bench` mode, where every frame is
//! synchronised with the GPU — the GPU execution time. The first frame is
//! reported separately: it shapes every line and rasterizes every glyph.
//!
//! With split panes each pane's instance build time is recorded too (the
//! per-pane part of `prepare`; the GPU work of a frame is not split per
//! pane), and frames showing at least [`SPLIT_PANES`] panes of at least
//! [`SPLIT_CELLS`] are judged against [`SPLIT_TARGET_MS`] the same way.

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

    /// CPU + GPU work for the frame; `None` when the GPU was not measured.
    pub fn total(&self) -> Option<Duration> {
        self.gpu.map(|gpu| self.cpu() + gpu)
    }
}

/// The M4 split check: this many panes …
pub const SPLIT_PANES: usize = 4;
/// … each at least this many cells (cols, rows) …
pub const SPLIT_CELLS: (u16, u16) = (120, 40);
/// … render within this (cpu + gpu, p99, strictly below).
pub const SPLIT_TARGET_MS: f64 = 4.0;

/// One pane of a frame: its size and how long its instances took.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneSample {
    pub cols: u16,
    pub rows: u16,
    pub build: Duration,
}

/// A pane's build times (panes are numbered in layout order).
#[derive(Clone, Debug, Default)]
struct PaneSeries {
    /// The size as last seen.
    cols: u16,
    rows: u16,
    build: Series,
}

/// Aggregates frame timings over a measurement window.
#[derive(Debug, Default)]
pub struct FrameStats {
    first: Option<FrameTiming>,
    panes: Vec<PaneSeries>,
    /// Frames that showed two panes or more.
    split_frames: usize,
    /// cpu + gpu of the frames that qualify for the split check.
    split_total: Series,
    /// The most panes a frame showed.
    max_panes: usize,
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
        self.record_frame(start, t, &[]);
    }

    /// [`Self::record`] with the frame's panes (layout order).
    pub fn record_frame(&mut self, start: Instant, t: FrameTiming, panes: &[PaneSample]) {
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
        // Only frames with GPU timing count towards the per-frame budget.
        if let Some(total) = t.total() {
            self.total.push(total);
        }
        if self.panes.len() < panes.len() {
            self.panes.resize_with(panes.len(), PaneSeries::default);
        }
        for (s, p) in self.panes.iter_mut().zip(panes) {
            s.cols = p.cols;
            s.rows = p.rows;
            s.build.push(p.build);
        }
        self.max_panes = self.max_panes.max(panes.len());
        if panes.len() >= 2 {
            self.split_frames += 1;
        }
        let (cols, rows) = SPLIT_CELLS;
        let qualifies =
            panes.len() >= SPLIT_PANES && panes.iter().all(|p| p.cols >= cols && p.rows >= rows);
        if let (true, Some(total)) = (qualifies, t.total()) {
            self.split_total.push(total);
        }
    }

    /// The split check's frames (cpu + gpu); `None` when no frame
    /// qualified.
    pub fn split_summary(&self) -> Option<Summary> {
        self.split_total.summary()
    }

    pub fn frames(&self) -> usize {
        self.cpu.len()
    }

    /// CPU + GPU frame time over the frames whose GPU time was measured;
    /// `None` when no frame was GPU-timed (no pass/fail possible).
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
        let gpu_frames = self.gpu.len();
        if gpu_frames == 0 {
            out.push_str("[stats]   gpu: not measured (use --bench)\n");
        } else if gpu_frames < self.frames() {
            out.push_str(&format!(
                "[stats]   gpu measured for {gpu_frames} of {} frames; the verdict uses those frames only\n",
                self.frames()
            ));
        }
        match (self.total_summary(), self.cpu.summary()) {
            (Some(sum), _) => {
                let verdict = if sum.p99 <= target_ms { "PASS" } else { "FAIL" };
                out.push_str(&format!(
                    "[stats]   target ≤ {target_ms} ms/frame (cpu + gpu, p99): {verdict} (avg {:.3} ms, p99 {:.3} ms, max {:.3} ms)\n",
                    sum.avg, sum.p99, sum.max
                ));
            }
            (None, Some(cpu)) => out.push_str(&format!(
                "[stats]   target ≤ {target_ms} ms/frame: not judged, cpu-only numbers (avg {:.3} ms, p99 {:.3} ms, max {:.3} ms); run --bench for GPU-inclusive timing\n",
                cpu.avg, cpu.p99, cpu.max
            )),
            (None, None) => {}
        }
        if self.split_frames > 0 {
            self.report_split(&mut out);
        }
        out
    }

    /// Per-pane build times and the M4 split verdict.
    fn report_split(&self, out: &mut String) {
        out.push_str(&format!(
            "[stats]   split panes: {} of {} frames showed 2+ panes (at most {})\n",
            self.split_frames,
            self.frames(),
            self.max_panes
        ));
        for (i, p) in self.panes.iter().enumerate() {
            if let Some(sum) = p.build.summary() {
                let name = format!("pane {} {}×{} build", i + 1, p.cols, p.rows);
                out.push_str(&format!("[stats]   {name:<34} {sum}\n"));
            }
        }
        let (cols, rows) = SPLIT_CELLS;
        let target = format!(
            "M4 split target < {SPLIT_TARGET_MS} ms/frame ({SPLIT_PANES} panes ≥ {cols}×{rows}, cpu + gpu, p99)"
        );
        match self.split_summary() {
            Some(sum) => {
                let verdict = if sum.p99 < SPLIT_TARGET_MS {
                    "PASS"
                } else {
                    "FAIL"
                };
                out.push_str(&format!(
                    "[stats]   {target}: {verdict} over {} frames (avg {:.3} ms, p99 {:.3} ms, max {:.3} ms)\n",
                    sum.n, sum.avg, sum.p99, sum.max
                ));
            }
            None => out.push_str(&format!(
                "[stats]   {target}: not judged (no GPU-timed frame had {SPLIT_PANES} panes of at least {cols}×{rows})\n"
            )),
        }
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

    fn cpu_only_frame() -> FrameTiming {
        FrameTiming {
            prepare: Duration::from_millis(1),
            encode: Duration::from_millis(1),
            ..Default::default()
        }
    }

    #[test]
    fn cpu_only_report_says_gpu_not_measured_and_gives_no_verdict() {
        let mut st = FrameStats::default();
        let t0 = Instant::now();
        for i in 0..10 {
            st.record(t0 + Duration::from_millis(8 * i), cpu_only_frame());
        }
        assert!(st.total_summary().is_none());
        let report = st.report("default path", 8.0);
        assert!(
            report.contains("gpu: not measured (use --bench)"),
            "{report}"
        );
        assert!(report.contains("not judged, cpu-only"), "{report}");
        assert!(
            !report.contains("PASS") && !report.contains("FAIL"),
            "{report}"
        );
        assert!(!report.contains("frame total"), "{report}");
    }

    #[test]
    fn split_frames_are_reported_per_pane_and_judged_at_four_panes() {
        let pane = |cols, rows, ms| PaneSample {
            cols,
            rows,
            build: Duration::from_micros(ms),
        };
        let frame = |gpu_ms| FrameTiming {
            prepare: Duration::from_micros(1500),
            encode: Duration::from_micros(500),
            gpu: Some(Duration::from_micros(gpu_ms)),
            ..Default::default()
        };
        let t0 = Instant::now();
        // One pane only: the report is the plain one.
        let mut st = FrameStats::default();
        for i in 0..5 {
            st.record_frame(
                t0 + Duration::from_millis(8 * i),
                frame(500),
                &[pane(120, 40, 300)],
            );
        }
        assert!(
            !st.report("single", 8.0).contains("split"),
            "no split lines"
        );

        let four = [
            pane(120, 40, 300),
            pane(121, 40, 310),
            pane(120, 41, 320),
            pane(130, 45, 330),
        ];
        let mut st = FrameStats::default();
        for i in 0..20 {
            st.record_frame(t0 + Duration::from_millis(8 * i), frame(500), &four);
        }
        // Too small panes do not count towards the verdict.
        st.record_frame(
            t0 + Duration::from_millis(200),
            frame(9000),
            &[pane(119, 40, 1); 4],
        );
        let sum = st.split_summary().unwrap();
        assert_eq!(sum.n, 19, "the first frame is kept apart");
        assert!((sum.p99 - 2.5).abs() < 1e-9);
        let report = st.report("split", 8.0);
        assert!(report.contains("pane 4 119×40 build"), "{report}");
        assert!(report.contains("M4 split target < 4 ms/frame"), "{report}");
        assert!(report.contains("PASS over 19 frames"), "{report}");

        // Slow frames: FAIL; p99 must be strictly below the target.
        let mut st = FrameStats::default();
        for i in 0..10 {
            st.record_frame(t0 + Duration::from_millis(8 * i), frame(2000), &four);
        }
        assert!(st.report("slow", 8.0).contains("FAIL over 9 frames"));

        // Split frames without GPU timing cannot be judged.
        let mut st = FrameStats::default();
        for i in 0..5 {
            st.record_frame(t0 + Duration::from_millis(8 * i), cpu_only_frame(), &four);
        }
        assert!(st
            .report("cpu", 8.0)
            .contains("not judged (no GPU-timed frame"));
    }

    #[test]
    fn partial_gpu_timing_judges_only_timed_frames() {
        let mut st = FrameStats::default();
        let t0 = Instant::now();
        for i in 0..11 {
            let mut t = cpu_only_frame();
            if i % 2 == 1 {
                t.gpu = Some(Duration::from_millis(20));
            }
            st.record(t0 + Duration::from_millis(8 * i), t);
        }
        let total = st.total_summary().unwrap();
        assert_eq!(total.n, 5);
        let report = st.report("mixed", 8.0);
        assert!(
            report.contains("gpu measured for 5 of 10 frames"),
            "{report}"
        );
        assert!(report.contains("FAIL"), "{report}");
    }
}
