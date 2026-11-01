//! App-owned progress for toggle animations.
//!
//! GPUI's `with_animation` keeps progress in element state, which is discarded
//! when an element unmounts. The shell is unmounted under the quick-look
//! overlay, so toggles driven that way would replay from the opposite state
//! when it returns. Keeping the start time and endpoints in `App` makes the
//! rendered progress independent of mount lifetime.

use std::time::{Duration, Instant};

use crate::ui::theme::eased_progress;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToggleMotion {
    epoch: u64,
    started_at: Instant,
    duration: Duration,
    from: f32,
    to: f32,
}

impl ToggleMotion {
    /// A settled value that renders immediately with no animation.
    pub(crate) fn settled(value: f32) -> Self {
        let value = value.clamp(0.0, 1.0);
        Self {
            epoch: 0,
            started_at: Instant::now(),
            duration: Duration::ZERO,
            from: value,
            to: value,
        }
    }

    /// Animates from the value currently on screen to `target`.
    pub(crate) fn animate_to(&mut self, target: f32, duration: Duration) {
        let target = target.clamp(0.0, 1.0);
        let from = self.value();
        if (from - target).abs() <= f32::EPSILON {
            self.epoch = self.epoch.wrapping_add(1);
            self.started_at = Instant::now();
            self.duration = Duration::ZERO;
            self.from = target;
            self.to = target;
            return;
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.started_at = Instant::now();
        self.duration = duration;
        self.from = from;
        self.to = target;
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn value(&self) -> f32 {
        self.value_at(Instant::now())
    }

    pub(crate) fn value_at(&self, now: Instant) -> f32 {
        let progress = eased_progress(self.started_at, self.duration, now);
        (self.from + (self.to - self.from) * progress).clamp(0.0, 1.0)
    }

    /// Duration for the frame-driver element. A settled motion returns zero so
    /// a remount paints once at the final value instead of replaying frames.
    pub(crate) fn driver_duration(&self) -> Duration {
        if self.is_active(Instant::now()) {
            self.duration
        } else {
            Duration::ZERO
        }
    }

    fn is_active(&self, now: Instant) -> bool {
        !self.duration.is_zero() && now.saturating_duration_since(self.started_at) < self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_motion_renders_its_value_without_frames() {
        let motion = ToggleMotion::settled(1.0);

        assert_eq!(motion.value(), 1.0);
        assert_eq!(motion.driver_duration(), Duration::ZERO);
    }

    #[test]
    fn motion_interpolates_from_the_current_value_to_the_target() {
        let mut motion = ToggleMotion::settled(0.0);
        motion.animate_to(1.0, Duration::from_millis(100));
        let start = motion.started_at;
        let epoch = motion.epoch();

        assert_eq!(motion.value_at(start), 0.0);
        assert_eq!(motion.value_at(start + Duration::from_millis(100)), 1.0);
        let middle = motion.value_at(start + Duration::from_millis(50));
        assert!(middle > 0.0 && middle < 1.0);
        assert_eq!(motion.epoch(), epoch);
    }

    #[test]
    fn retargeting_starts_from_the_rendered_value() {
        let mut motion = ToggleMotion::settled(0.0);
        motion.animate_to(1.0, Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(20));

        motion.animate_to(0.0, Duration::from_millis(100));

        assert!(motion.from > 0.0 && motion.from < 1.0);
        assert_eq!(motion.to, 0.0);
        assert_eq!(motion.value_at(motion.started_at), motion.from);
    }
}
