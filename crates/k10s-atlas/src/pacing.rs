#[derive(Debug, Default)]
pub struct FramePacer {
    continuous: bool,
}

impl FramePacer {
    pub fn begin_frame(&mut self) -> bool {
        std::mem::replace(&mut self.continuous, false)
    }

    pub fn request_frame(&mut self) {
        self.continuous = true;
    }

    pub fn frame_requested(&self) -> bool {
        self.continuous
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demand_latches_one_frame() {
        let mut p = FramePacer::default();
        assert!(!p.begin_frame());
        p.request_frame();
        assert!(p.frame_requested());
        assert!(p.begin_frame());
        assert!(!p.frame_requested());
        assert!(!p.begin_frame());
    }
}
