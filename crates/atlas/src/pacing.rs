//! Whether the next frame was asked for.
//!
//! One bit, consumed by the frame it belongs to. A window that animates has to
//! ask for the frame after the one it is painting, and the whole of this
//! crate's idle claim -- zero paints while nothing moves -- is that asking is
//! something a frame does deliberately and once. So the latch is cleared as the
//! frame begins rather than after it ends: anything still moving re-arms it
//! while painting, and anything that has finished simply does not, with no
//! cancellation to get wrong and no way for a paint that aborted half way to
//! leave the window spinning.
//!
//! [`FlyTo::step`](crate::motion::FlyTo::step) is the other half of the
//! vocabulary: a [`Step`](crate::motion::Step) that owes a frame is what a
//! caller re-arms this latch from.

/// The request bit for the next frame.
#[derive(Debug, Default)]
pub struct FramePacer {
    continuous: bool,
}

impl FramePacer {
    /// Take the request and clear it, saying whether this frame was asked for.
    ///
    /// Called once at the top of a paint. Whatever is still animating has to ask
    /// again during that paint, or this was the last frame.
    pub fn begin_frame(&mut self) -> bool {
        std::mem::replace(&mut self.continuous, false)
    }

    /// Ask for one more frame after this one. Idempotent within a frame.
    pub fn request_frame(&mut self) {
        self.continuous = true;
    }

    /// Whether a frame is currently owed, without consuming the request.
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
