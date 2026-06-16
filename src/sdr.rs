//! RTL-SDR device discovery, wait loop, and IQ capture thread.

use crate::app::AppEvent;
use crate::demod::{configure_sdr, optimal_settings, DemodPipeline, MPX_SAMPLE_RATE, RadioConfig};
use rtl_sdr_rs::{RtlSdr, DEFAULT_BUF_LENGTH};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct SdrHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SdrHandle {
    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn device_count() -> usize {
    RtlSdr::get_device_count().unwrap_or(0)
}

pub fn wait_for_device() -> Result<(), String> {
    let frames = ['|', '/', '-', '\\'];
    let mut i = 0usize;
    loop {
        let count = device_count();
        if count > 0 {
            eprintln!("\rSDR detected ({count} device(s)).          ");
            return Ok(());
        }
        eprint!("\rWaiting for SDR... {}  ", frames[i % frames.len()]);
        i += 1;
        thread::sleep(Duration::from_secs(1));
    }
}

fn format_sdr_error(e: &rtl_sdr_rs::error::RtlsdrError) -> String {
    let msg = format!("{e}");
    if msg.contains("Busy") || msg.contains("busy") {
        format!(
            "{msg}\n\nLinux: kernel DVB driver may be claiming the dongle.\n\
             Try: sudo rmmod rtl2832_sdr dvb_usb_rtl28xxu rtl2832 rtl8xxxu\n\
             Or add a udev rule (see README)."
        )
    } else {
        msg
    }
}

pub fn start_capture(
    freq_hz: u32,
    ppm: i32,
    gain_db: i32,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    shutdown: Arc<AtomicBool>,
) -> Result<SdrHandle, String> {
    let radio = optimal_settings(freq_hz, MPX_SAMPLE_RATE);
    let shutdown_thread = shutdown.clone();
    let thread = thread::Builder::new()
        .name("sdr-capture".into())
        .spawn(move || capture_loop(freq_hz, ppm, gain_db, radio, event_tx, shutdown_thread))
        .map_err(|e| e.to_string())?;

    Ok(SdrHandle {
        shutdown: shutdown.clone(),
        thread: Some(thread),
    })
}

fn capture_loop(
    freq_hz: u32,
    ppm: i32,
    gain_db: i32,
    radio: RadioConfig,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let mut sdr = match RtlSdr::open_first_available() {
            Ok(s) => s,
            Err(e) => {
                let _ = event_tx.send(AppEvent::SdrDisconnected(format_sdr_error(&e)));
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        if ppm != 0 {
            let _ = sdr.set_freq_correction(ppm);
        }

        if let Err(e) = configure_sdr(&mut sdr, &radio, gain_db) {
            let _ = event_tx.send(AppEvent::SdrDisconnected(format_sdr_error(&e)));
            thread::sleep(Duration::from_secs(1));
            continue;
        }

        let _ = event_tx.send(AppEvent::SdrConnected {
            freq_hz,
            sample_rate: radio.capture_rate,
        });

        let mut demod = DemodPipeline::new(radio.downsample);
        let mut buf = vec![0u8; DEFAULT_BUF_LENGTH];

        while !shutdown.load(Ordering::Relaxed) {
            match sdr.read_sync(&mut buf) {
                Ok(n) if n >= DEFAULT_BUF_LENGTH => {
                    let frame = demod.process_iq(&buf);
                    let (left, right) = normalize_stereo(&frame.left, &frame.right);
                    let peak_l = crate::demod::peak_dbfs(&left);
                    let peak_r = crate::demod::peak_dbfs(&right);
                    let _ = event_tx.send(AppEvent::StereoData {
                        left,
                        right,
                        is_stereo: frame.is_stereo,
                        peak_l,
                        peak_r,
                    });
                }
                Ok(_) => {
                    let _ = event_tx.send(AppEvent::SdrDisconnected(
                        "Short read from SDR — device may have been unplugged.".into(),
                    ));
                    break;
                }
                Err(e) => {
                    let _ = event_tx.send(AppEvent::SdrDisconnected(format_sdr_error(&e)));
                    break;
                }
            }
        }

        let _ = sdr.close();
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

/// Ring buffer for audio output and display sampling.
pub struct SampleRing {
    left: Vec<f32>,
    right: Vec<f32>,
    read_pos: usize,
}

impl SampleRing {
    pub fn new() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
            read_pos: 0,
        }
    }

    pub fn push_frame(&mut self, left: &[f32], right: &[f32]) {
        self.left.extend_from_slice(left);
        self.right.extend_from_slice(right);
        const MAX: usize = MPX_SAMPLE_RATE as usize * 2;
        if self.left.len() > MAX {
            let drop = self.left.len() - MAX;
            self.left.drain(..drop);
            self.right.drain(..drop);
            self.read_pos = self.read_pos.saturating_sub(drop);
        }
    }

    pub fn read_interleaved(&mut self, out: &mut [f32]) -> usize {
        let available = self.left.len().saturating_sub(self.read_pos);
        let frames = available.min(out.len() / 2);
        for i in 0..frames {
            out[i * 2] = self.left[self.read_pos + i];
            out[i * 2 + 1] = self.right[self.read_pos + i];
        }
        self.read_pos += frames;
        if self.read_pos > self.left.len() / 2 {
            self.left.drain(..self.read_pos);
            self.right.drain(..self.read_pos);
            self.read_pos = 0;
        }
        frames * 2
    }
}

pub type SharedRing = Arc<Mutex<SampleRing>>;

pub fn new_shared_ring() -> SharedRing {
    Arc::new(Mutex::new(SampleRing::new()))
}

fn normalize_stereo(left: &[f32], right: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let peak = left
        .iter()
        .chain(right.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);
    let scale = if peak > 1e-6 { 0.85 / peak } else { 1.0 };
    let l = left.iter().map(|s| s * scale).collect();
    let r = right.iter().map(|s| s * scale).collect();
    (l, r)
}
