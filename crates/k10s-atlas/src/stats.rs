use std::time::Instant;

const WINDOW: usize = 240;

const IDLE_GAP_MS: f32 = 500.0;

struct Ring {
    buf: [f32; WINDOW],
    len: usize,
    head: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Ring {
            buf: [0.0; WINDOW],
            len: 0,
            head: 0,
        }
    }
}

impl Ring {
    fn clear(&mut self) {
        self.len = 0;
        self.head = 0;
    }

    fn push(&mut self, v: f32) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % WINDOW;
        if self.len < WINDOW {
            self.len += 1;
        }
    }

    fn sorted_into<'a>(&self, scratch: &'a mut [f32; WINDOW]) -> &'a [f32] {
        let start = (self.head + WINDOW - self.len) % WINDOW;
        let out = &mut scratch[..self.len];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.buf[(start + i) % WINDOW];
        }
        out.sort_unstable_by(f32::total_cmp);
        out
    }
}

#[derive(Default)]
pub struct FrameStats {
    last_frame: Option<Instant>,
    intervals: Ring,
    cpu: Ring,
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
                self.intervals.push(ms);
            }
        }
        self.last_frame = Some(now);
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn end_cpu(&mut self, frame_start: Instant) {
        self.cpu.push(frame_start.elapsed().as_secs_f32() * 1000.0);
    }

    pub fn frame_percentiles(&self) -> (f32, f32, f32) {
        let mut scratch = [0.0f32; WINDOW];
        let s = self.intervals.sorted_into(&mut scratch);
        (pct(s, 0.50), pct(s, 0.95), pct(s, 0.99))
    }

    pub fn cpu_percentiles(&self) -> (f32, f32) {
        let mut scratch = [0.0f32; WINDOW];
        let s = self.cpu.sorted_into(&mut scratch);
        (pct(s, 0.50), pct(s, 0.99))
    }
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
    fn ring_keeps_only_the_last_window_samples() {
        let mut st = FrameStats::default();
        for i in 0..WINDOW + 50 {
            st.end_cpu(Instant::now() - Duration::from_millis(i as u64));
        }
        let (p50, p99) = st.cpu_percentiles();
        assert!(p50 > 0.0 && p99 >= p50);

        let mut ring = Ring::default();
        for i in 0..WINDOW + 3 {
            ring.push(i as f32);
        }
        assert_eq!(ring.len, WINDOW);
        let mut scratch = [0.0f32; WINDOW];
        let s = ring.sorted_into(&mut scratch);
        assert_eq!(s.len(), WINDOW);
        assert_eq!(s[0], 3.0, "oldest three samples must be evicted");
        assert_eq!(s[WINDOW - 1], (WINDOW + 2) as f32);
    }

    #[test]
    fn percentiles_on_empty_and_single_sample() {
        let st = FrameStats::default();
        assert_eq!(st.frame_percentiles(), (0.0, 0.0, 0.0));
        assert_eq!(st.cpu_percentiles(), (0.0, 0.0));

        let mut ring = Ring::default();
        ring.push(7.5);
        let mut scratch = [0.0f32; WINDOW];
        let s = ring.sorted_into(&mut scratch);
        assert_eq!(s, [7.5]);
        assert_eq!(pct(s, 0.50), 7.5);
        assert_eq!(pct(s, 0.99), 7.5);
    }

    #[test]
    fn reset_clears_both_rings() {
        let mut st = FrameStats::default();
        st.end_cpu(Instant::now() - Duration::from_millis(5));
        st.begin_frame(Instant::now(), true);
        st.begin_frame(Instant::now() + ms(10), true);
        st.reset();
        assert_eq!(st.frame_percentiles(), (0.0, 0.0, 0.0));
        assert_eq!(st.cpu_percentiles(), (0.0, 0.0));
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
