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
        gain_tenths: i32,
    },
    SdrDisconnected(String),
    FilePlaying {
        path: String,
        sample_rate: u32,
        loop_playback: bool,
    },
    FileFinished,
    StereoData {
        scope_left: Vec<f32>,
        scope_right: Vec<f32>,
        peak_l: f32,
        peak_r: f32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputSource {
    Sdr,
    File,
}

pub struct AppState {
    pub input_source: InputSource,
    pub file_path: String,
    pub file_loop: bool,
    pub freq_hz: u32,
    pub ppm: i32,
    pub sdr_connected: bool,
    pub peak_l: f32,
    pub peak_r: f32,
    pub audio_device: String,
    pub audio_rate: u32,
    pub requested_rate: u32,
    pub status_message: String,
    pub gain_index: usize,
    pub gain_tenths: i32,
    pub mono_only: bool,
}

impl AppState {
    pub fn new(freq_hz: u32, ppm: i32) -> Self {
        Self {
            input_source: InputSource::Sdr,
            file_path: String::new(),
            file_loop: true,
            freq_hz,
            ppm,
            sdr_connected: false,
            peak_l: -120.0,
            peak_r: -120.0,
            audio_device: "-".into(),
            audio_rate: 0,
            requested_rate: 0,
            status_message: String::new(),
            gain_index: 0,
            gain_tenths: -1,
            mono_only: false,
        }
    }

    pub fn gain_label(&self) -> String {
        let target = GAIN_STEPS[self.gain_index];
        if target < 0 {
            "auto".to_string()
        } else {
            format!("{:.1} dB", target as f64 / 10.0)
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

/// Tuner gain targets in tenths of a dB (-1 = hardware AGC).
pub const GAIN_STEPS: [i32; 4] = [-1, 0, 200, 400];
