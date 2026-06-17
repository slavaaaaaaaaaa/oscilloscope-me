//! Keyboard control constants aligned with oscope-me.

pub const VOLUME_STEP: f32 = 0.02;
pub const VOLUME_MAX: f32 = 8.0;
pub const FILE_SEEK_SECS: f64 = 10.0;
pub const TUNE_STEP_MHZ: f64 = 0.1;
pub const TUNE_COARSE_MHZ: f64 = 1.0;

/// Drop OS key-repeat for volume keys (act once per physical press).
pub struct RepeatFilter {
    held_key: Option<char>,
    held_since: std::time::Instant,
    release_gap: std::time::Duration,
}

impl RepeatFilter {
    pub fn new() -> Self {
        Self {
            held_key: None,
            held_since: std::time::Instant::now(),
            release_gap: std::time::Duration::from_millis(150),
        }
    }

    pub fn filter(&mut self, key: Option<char>, now: std::time::Instant) -> Option<char> {
        if self
            .held_key
            .is_some_and(|_| now.duration_since(self.held_since) > self.release_gap)
        {
            self.held_key = None;
        }
        let key = key?;
        if is_volume_key(key) {
            if self.held_key == Some(key) {
                return None;
            }
            self.held_key = Some(key);
            self.held_since = now;
            return Some(key);
        }
        self.held_key = None;
        Some(key)
    }
}

pub fn is_volume_key(c: char) -> bool {
    matches!(c, '+' | '=' | '-' | '_')
}

pub fn is_volume_up(c: char) -> bool {
    matches!(c, '+' | '=')
}

pub fn is_volume_down(c: char) -> bool {
    matches!(c, '-' | '_')
}
