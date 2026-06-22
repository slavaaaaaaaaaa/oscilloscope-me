//! Keyboard control constants aligned with oscope-me.

use crossterm::event::KeyCode;
use std::time::{Duration, Instant};

pub const VOLUME_STEP: f32 = 0.02;
pub const VOLUME_MAX: f32 = 8.0;
pub const FILE_SEEK_SECS: f64 = 10.0;
pub const TUNE_STEP_MHZ: f64 = 0.1;
pub const TUNE_COARSE_MHZ: f64 = 1.0;

/// Drop OS key-repeat for held keys (act once per physical press).
///
/// Terminals often deliver autorepeat as repeated `Press` events with no
/// corresponding `Release`. We latch the last accepted key and track the most
/// recent event time for that key; the latch clears on `Release` or after an
/// idle gap with no further events (`tick`).
pub struct RepeatFilter {
    held_key: Option<KeyCode>,
    last_seen: Instant,
    release_gap: Duration,
}

impl RepeatFilter {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            held_key: None,
            last_seen: now,
            release_gap: Duration::from_millis(150),
        }
    }

    /// Call once per main-loop iteration so a released key can be pressed again.
    pub fn tick(&mut self, now: Instant) {
        if self
            .held_key
            .is_some_and(|_| now.duration_since(self.last_seen) > self.release_gap)
        {
            self.held_key = None;
        }
    }

    pub fn release(&mut self, code: KeyCode) {
        if self.held_key == Some(code) {
            self.held_key = None;
        }
    }

    /// Track a filtered key event that should not trigger an action (e.g. OS repeat).
    pub fn observe(&mut self, code: KeyCode, now: Instant) {
        if Self::is_repeat_filtered(code) {
            self.last_seen = now;
        }
    }

    /// Returns true if this key event should trigger an action.
    pub fn allow(&mut self, code: KeyCode, now: Instant) -> bool {
        if !Self::is_repeat_filtered(code) {
            self.held_key = None;
            return true;
        }
        self.last_seen = now;
        if self.held_key == Some(code) {
            return false;
        }
        self.held_key = Some(code);
        true
    }

    fn is_repeat_filtered(code: KeyCode) -> bool {
        matches!(
            code,
            KeyCode::Char('+')
                | KeyCode::Char('=')
                | KeyCode::Char('-')
                | KeyCode::Char('_')
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('.')
                | KeyCode::Char(',')
                | KeyCode::Char('>')
                | KeyCode::Char('<')
        )
    }
}

pub fn is_volume_up(c: char) -> bool {
    matches!(c, '+' | '=')
}

pub fn is_volume_down(c: char) -> bool {
    matches!(c, '-' | '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_immediate_repeat() {
        let mut f = RepeatFilter::new();
        let t0 = Instant::now();
        assert!(f.allow(KeyCode::Down, t0));
        assert!(!f.allow(KeyCode::Down, t0));
    }

    fn advance(f: &mut RepeatFilter, t0: Instant, ms: u64) -> Instant {
        let now = t0 + Duration::from_millis(ms);
        f.tick(now);
        now
    }

    #[test]
    fn allows_same_key_after_idle() {
        let mut f = RepeatFilter::new();
        let t0 = Instant::now();
        assert!(f.allow(KeyCode::Down, t0));
        assert!(!f.allow(KeyCode::Down, t0 + Duration::from_millis(50)));
        let t1 = advance(&mut f, t0, 220);
        assert!(f.allow(KeyCode::Down, t1));
    }

    #[test]
    fn release_clears_held_key() {
        let mut f = RepeatFilter::new();
        let t0 = Instant::now();
        assert!(f.allow(KeyCode::Up, t0));
        assert!(!f.allow(KeyCode::Up, t0));
        f.release(KeyCode::Up);
        assert!(f.allow(KeyCode::Up, t0));
    }

    #[test]
    fn held_key_survives_repeats_until_idle() {
        let mut f = RepeatFilter::new();
        let t0 = Instant::now();
        assert!(f.allow(KeyCode::Down, t0));
        for ms in [40, 80, 120, 160] {
            let now = t0 + Duration::from_millis(ms);
            assert!(!f.allow(KeyCode::Down, now));
            f.tick(now);
        }
        let late = t0 + Duration::from_millis(200);
        f.tick(late);
        assert!(!f.allow(KeyCode::Down, late));
        let idle = t0 + Duration::from_millis(400);
        f.tick(idle);
        assert!(f.allow(KeyCode::Down, idle));
    }
}
