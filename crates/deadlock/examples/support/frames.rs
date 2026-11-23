//! Frame-interval sampling for the spikes: call `tick` from `render` while a
//! scripted scroll keeps the window redrawing, then read the printed summary.
//! `draw` is GPUI's own CPU time in `Window::draw` (profiler frame trace, enabled by the
//! gpui-fps HUD); `interval` is the time between frames, which vsync caps from below.

use std::time::{Duration, Instant};

use gpui_kit::{FrameEvent, FrameTimingCollector};

pub struct FrameStats {
    label: &'static str,
    window: Duration,
    last: Option<Instant>,
    start: Option<Instant>,
    samples: Vec<Duration>,
    collector: FrameTimingCollector,
    done: bool,
}

impl FrameStats {
    pub fn new(label: &'static str, window: Duration) -> Self {
        Self {
            label,
            window,
            last: None,
            start: None,
            samples: Vec::new(),
            collector: FrameTimingCollector::new(),
            done: false,
        }
    }

    /// Records one frame. Returns true while sampling; prints once when the window ends.
    pub fn tick(&mut self) -> bool {
        if self.done {
            return false;
        }
        let now = Instant::now();
        if let Some(last) = self.last {
            self.samples.push(now - last);
        }
        self.last = Some(now);
        let start = *self.start.get_or_insert(now);
        if now - start < self.window {
            return true;
        }
        self.done = true;
        self.samples.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.;
        let pct = |v: &[Duration], p: f64| ms(v[((v.len() - 1) as f64 * p) as usize]);
        let mut draws: Vec<_> = self
            .collector
            .collect_unseen()
            .into_iter()
            .filter_map(|e| match e {
                FrameEvent::Draw(t) => Some(t),
                FrameEvent::Present(_) => None,
            })
            .collect();
        if !draws.is_empty() {
            let k = draws.len() as u32;
            let phase = |f: fn(&gpui_kit::DrawPhases) -> Duration| {
                ms(draws.iter().map(|t| f(&t.phases)).sum::<Duration>() / k)
            };
            let (layout, prepaint, paint) = (
                phase(|p| p.request_layout),
                phase(|p| p.prepaint),
                phase(|p| p.paint),
            );
            let mut d: Vec<_> = draws.drain(..).map(|t| t.draw_end - t.draw_start).collect();
            d.sort();
            println!(
                "[{}] draw (CPU): p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms; mean request_layout {layout:.2} / prepaint {prepaint:.2} / paint {paint:.2} ms",
                self.label,
                pct(&d, 0.5),
                pct(&d, 0.95),
                ms(d[d.len() - 1]),
            );
        }
        let n = self.samples.len();
        let over = self
            .samples
            .iter()
            .filter(|d| **d > Duration::from_micros(25_000))
            .count();
        println!(
            "[{}] interval: {n} frames in {:.1} s: p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms, {over} frames > 25 ms",
            self.label,
            (now - start).as_secs_f64(),
            pct(&self.samples, 0.5),
            pct(&self.samples, 0.95),
            pct(&self.samples, 0.99),
            ms(self.samples[n - 1]),
        );
        false
    }
}
