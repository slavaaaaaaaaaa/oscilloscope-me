//! Application state and event types.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct StereoSample {
    pub x: f32,
    pub y: f32,
}

pub enum AppEvent {
    SdrConnected {
        freq_hz: u32,
        sample_rate: u32,
    },
    SdrDisconnected(String),
    StereoData {
        left: Vec<f32>,
        right: Vec<f32>,
        is_stereo: bool,
        peak_l: f32,
        peak_r: f32,
    },
}

pub struct AppState {
    pub freq_hz: u32,
    pub ppm: i32,
    pub sdr_connected: bool,
    pub is_stereo: bool,
    pub peak_l: f32,
    pub peak_r: f32,
    pub audio_device: String,
    pub audio_rate: u32,
    pub display_samples: Vec<StereoSample>,
    pub status_message: String,
    pub gain_index: usize,
}

impl AppState {
    pub fn new(freq_hz: u32, ppm: i32) -> Self {
        Self {
            freq_hz,
            ppm,
            sdr_connected: false,
            is_stereo: false,
            peak_l: -120.0,
            peak_r: -120.0,
            audio_device: "—".into(),
            audio_rate: 0,
            display_samples: Vec::new(),
            status_message: String::new(),
            gain_index: 0,
        }
    }
}

pub struct ShutdownFlag(Arc<AtomicBool>);

impl ShutdownFlag {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn handle(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }

    pub fn set(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

pub const GAIN_STEPS: [i32; 4] = [-1, 0, 20, 40]; // -1 = auto
