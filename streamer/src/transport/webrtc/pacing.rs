//! Spread each video frame over a short, bounded window. This does not change
//! the encoder bitrate or claim to provide a sender congestion controller.
use std::{sync::atomic::{AtomicU32, AtomicU64, Ordering}, time::Duration};

#[derive(Default)]
pub struct VideoPacing {
    fps: AtomicU32,
    receiver_bps: AtomicU64,
    feedback_at_ms: AtomicU64,
}

impl VideoPacing {
    pub fn set_fps(&self, fps: u32) { self.fps.store(fps.clamp(1, 240), Ordering::Relaxed); }

    pub fn observe_receiver_bitrate(&self, bps: f32, now_ms: u64) {
        if bps.is_finite() && bps > 0.0 {
            self.receiver_bps.store((bps as u64).clamp(64_000, 1_000_000_000), Ordering::Relaxed);
            self.feedback_at_ms.store(now_ms, Ordering::Relaxed);
        }
    }

    pub fn receiver_bps(&self, now_ms: u64) -> u64 {
        if now_ms.saturating_sub(self.feedback_at_ms.load(Ordering::Relaxed)) > 3000 { return 0; }
        self.receiver_bps.load(Ordering::Relaxed)
    }

    pub fn window(&self, bytes: usize, now_ms: u64) -> Duration {
        let fps = self.fps.load(Ordering::Relaxed).max(1);
        let frame_us = 1_000_000 / u64::from(fps);
        let base_us = (frame_us / 2).min(8000);
        let cap_us = (frame_us * 3 / 4).min(12_000);
        let bps = self.receiver_bps(now_ms);
        let requested = if bps > 0 {
            // Wire overhead and 25% headroom; cap the window instead of queuing
            // an arbitrarily large frame behind an unrealistically low REMB.
            (bytes as u64).saturating_mul(10_000_000) / bps
        } else { base_us };
        Duration::from_micros(requested.max(base_us).min(cap_us))
    }
}

pub fn offset(window: Duration, bytes_before: usize, total_bytes: usize) -> Duration {
    if total_bytes == 0 { return Duration::ZERO; }
    Duration::from_nanos((window.as_nanos() * bytes_before as u128 / total_bytes as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_spreads_60fps_frames_without_adding_a_frame_of_delay() {
        let p = VideoPacing::default(); p.set_fps(60);
        assert_eq!(p.window(25_000, 0), Duration::from_millis(8));
        assert_eq!(offset(Duration::from_millis(8), 2500, 10_000), Duration::from_millis(2));
        assert_eq!(offset(Duration::from_millis(8), 0, 0), Duration::ZERO);
    }
    #[test]
    fn low_feedback_cannot_create_unbounded_latency_and_stale_feedback_expires() {
        let p = VideoPacing::default(); p.set_fps(60);
        p.observe_receiver_bitrate(64_000.0, 100);
        assert_eq!(p.window(2_000_000, 100), Duration::from_millis(12));
        assert_eq!(p.receiver_bps(3200), 0);
        assert_eq!(p.window(2_000_000, 3200), Duration::from_millis(8));
        p.observe_receiver_bitrate(f32::NAN, 3300);
        assert_eq!(p.receiver_bps(3300), 0);
    }
    #[test]
    fn high_fps_and_packet_order_are_preserved() {
        let p = VideoPacing::default(); p.set_fps(240);
        p.observe_receiver_bitrate(100_000.0, 0);
        assert!(p.window(200_000, 0) < Duration::from_micros(4166));
        let times: Vec<_> = (0..100).map(|i| offset(p.window(100_000, 0), i * 1000, 100_000)).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]));
    }
}
