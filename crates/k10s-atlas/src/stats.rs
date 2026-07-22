use std::collections::VecDeque;
use std::time::Instant;

const WINDOW: usize = 240;

const IDLE_GAP_MS: f32 = 500.0;

#[derive(Default)]
pub struct FrameStats {
    last_frame: Option<Instant>,
    intervals: VecDeque<f32>,
    cpu: VecDeque<f32>,
    frames: u64,
    pub quads: usize,
    pub glyphs: usize,
    pub edges: usize,
    pub icons: usize,
    pub sats: usize,
    pub curves: usize,
    pub curves_dropped: usize,
    pub bg_cells: usize,
    pub drawn: (usize, usize, usize),
    pub labels_dropped: usize,
    pub icons_dropped: usize,
}

impl FrameStats {
    pub fn reset(&mut self) {
        self.intervals.clear();
        self.cpu.clear();
    }

    pub fn begin_frame(&mut self, now: Instant, continuous: bool) {
        self.frames += 1;
        if let Some(last) = self.last_frame {
            let ms = (now - last).as_secs_f32() * 1000.0;
            if continuous || ms < IDLE_GAP_MS {
                push(&mut self.intervals, ms);
            }
        }
        self.last_frame = Some(now);
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn end_cpu(&mut self, frame_start: Instant) {
        push(&mut self.cpu, frame_start.elapsed().as_secs_f32() * 1000.0);
    }

    pub fn frame_percentiles(&self) -> (f32, f32, f32) {
        let s = sorted(&self.intervals);
        (pct(&s, 0.50), pct(&s, 0.95), pct(&s, 0.99))
    }

    pub fn cpu_percentiles(&self) -> (f32, f32) {
        let s = sorted(&self.cpu);
        (pct(&s, 0.50), pct(&s, 0.99))
    }
}

fn push(q: &mut VecDeque<f32>, v: f32) {
    if q.len() == WINDOW {
        q.pop_front();
    }
    q.push_back(v);
}

fn sorted(q: &VecDeque<f32>) -> Vec<f32> {
    let mut v: Vec<f32> = q.iter().copied().collect();
    v.sort_by(f32::total_cmp);
    v
}

fn pct(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f32 - 1.0) * p).round() as usize;
    sorted[i]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn damage_gaps_stay_out_of_cadence_percentiles() {
        let mut st = FrameStats::default();
        let t0 = Instant::now();
        st.begin_frame(t0, false);
        st.begin_frame(t0 + ms(10), false);
        st.begin_frame(t0 + ms(5010), false);
        st.begin_frame(t0 + ms(5020), false);
        assert_eq!(st.frames(), 4);
        let (p50, _, p99) = st.frame_percentiles();
        assert!((p50 - 10.0).abs() < 0.5, "p50 {p50}");
        assert!(p99 < 100.0, "idle gap leaked into p99: {p99}");
    }

    #[test]
    fn continuous_stalls_do_count() {
        let mut st = FrameStats::default();
        let t0 = Instant::now();
        st.begin_frame(t0, true);
        st.begin_frame(t0 + ms(10), true);
        st.begin_frame(t0 + ms(910), true);
        let (_, _, p99) = st.frame_percentiles();
        assert!(p99 > 800.0, "continuous stall must count: p99 {p99}");
    }
}
