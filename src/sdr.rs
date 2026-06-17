//! RTL-SDR device discovery, wait loop, and IQ capture thread.

use crate::app::AppEvent;
use crate::audio::StereoResampler;
use crate::demod::{
    configure_sdr, optimal_settings, DemodPipeline, AUDIO_SAMPLE_RATE, MPX_SAMPLE_RATE,
};
use crate::display::decimate_scope_pair;
use rtl_sdr_rs::{RtlSdr, DEFAULT_BUF_LENGTH};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

enum SdrCommand {
    SetFreq(u32),
    SetGain(i32),
    SetMono(bool),
    SetPpm(i32),
    SetDeemphasis(u8),
}

pub struct SdrHandle {
    stop: Arc<AtomicBool>,
    cmd_tx: crossbeam_channel::Sender<SdrCommand>,
    thread: Option<thread::JoinHandle<()>>,
}

impl SdrHandle {
    pub fn set_freq(&self, freq_hz: u32) {
        let _ = self.cmd_tx.send(SdrCommand::SetFreq(freq_hz));
    }

    pub fn set_gain(&self, gain_db: i32) {
        let _ = self.cmd_tx.send(SdrCommand::SetGain(gain_db));
    }

    pub fn set_mono(&self, mono: bool) {
        let _ = self.cmd_tx.send(SdrCommand::SetMono(mono));
    }

    pub fn set_ppm(&self, ppm: i32) {
        let _ = self.cmd_tx.send(SdrCommand::SetPpm(ppm));
    }

    pub fn set_deemphasis(&self, us: u8) {
        let _ = self.cmd_tx.send(SdrCommand::SetDeemphasis(us));
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn device_count() -> usize {
    RtlSdr::get_device_count().unwrap_or(0)
}

pub fn wait_for_device() -> Result<(), String> {
    let frames = ['|', '/', '-', '\\'];
    let mut i = 0usize;
    loop {
        let count = device_count();
        if count > 0 {
            eprintln!("\rSDR detected ({count} device(s)).          ");
            // Enumeration briefly opens the dongle; give USB a moment to release
            // before audio init and the real capture open.
            thread::sleep(Duration::from_millis(300));
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
            "{msg}\n\n\
             Another process may still have the dongle open (e.g. a previous run):\n\
               pkill -f oscilloscope-me\n\
             Unplug/replug the dongle, then try again.\n\n\
             Linux: kernel DVB driver may also be claiming it:\n\
               sudo rmmod rtl2832_sdr dvb_usb_rtl28xxu rtl2832 rtl8xxxu\n\
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
    mono_only: bool,
    deemphasis_us: u8,
    device_rate: u32,
    ring: SharedRing,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    app_shutdown: Arc<AtomicBool>,
) -> Result<SdrHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let stop_thread = stop.clone();
    let app_shutdown_thread = app_shutdown.clone();
    let thread = thread::Builder::new()
        .name("sdr-capture".into())
        .spawn(move || {
            capture_loop(
                freq_hz,
                ppm,
                gain_db,
                mono_only,
                deemphasis_us,
                device_rate,
                ring,
                event_tx,
                cmd_rx,
                stop_thread,
                app_shutdown_thread,
            )
        })
        .map_err(|e| e.to_string())?;

    Ok(SdrHandle {
        stop,
        cmd_tx,
        thread: Some(thread),
    })
}

fn capture_loop(
    freq_hz: u32,
    mut ppm: i32,
    gain_db: i32,
    mut mono_only: bool,
    mut deemphasis_us: u8,
    device_rate: u32,
    ring: SharedRing,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    cmd_rx: crossbeam_channel::Receiver<SdrCommand>,
    stop: Arc<AtomicBool>,
    app_shutdown: Arc<AtomicBool>,
) {
    let mut resampler = StereoResampler::new(AUDIO_SAMPLE_RATE, device_rate);
    let mut freq_hz = freq_hz;
    let mut gain_db = gain_db;

    loop {
        if stop.load(Ordering::Relaxed) || app_shutdown.load(Ordering::Relaxed) {
            break;
        }

        let mut sdr = match RtlSdr::open_with_index(0) {
            Ok(s) => s,
            Err(e) => {
                let busy = format!("{e}").to_ascii_lowercase().contains("busy");
                let _ = event_tx.send(AppEvent::SdrDisconnected(format_sdr_error(&e)));
                thread::sleep(if busy {
                    Duration::from_secs(2)
                } else {
                    Duration::from_secs(1)
                });
                continue;
            }
        };

        if ppm != 0 {
            let _ = sdr.set_freq_correction(ppm);
        }

        let (radio, mut demod_config) = optimal_settings(freq_hz, MPX_SAMPLE_RATE);
        if let Err(e) = configure_sdr(&mut sdr, &radio, gain_db) {
            let _ = event_tx.send(AppEvent::SdrDisconnected(format_sdr_error(&e)));
            thread::sleep(Duration::from_secs(1));
            continue;
        }

        let _ = event_tx.send(AppEvent::SdrConnected {
            freq_hz,
            gain_tenths: gain_db,
        });

        let mut demod = DemodPipeline::new(demod_config, mono_only, deemphasis_us);
        let mut buf = vec![0u8; DEFAULT_BUF_LENGTH];

        while !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed) {
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    SdrCommand::SetFreq(new_freq) => {
                        freq_hz = new_freq;
                        let (radio, fresh) = optimal_settings(freq_hz, MPX_SAMPLE_RATE);
                        demod_config = fresh;
                        if sdr.set_center_freq(radio.capture_freq).is_ok() {
                            let _ = sdr.reset_buffer();
                            demod = DemodPipeline::new(demod_config, mono_only, deemphasis_us);
                            if let Ok(mut r) = ring.lock() {
                                r.clear();
                            }
                            let _ = event_tx.send(AppEvent::SdrConnected {
                                freq_hz,
                                gain_tenths: gain_db,
                            });
                        }
                    }
                    SdrCommand::SetGain(new_gain) => {
                        gain_db = new_gain;
                        let (radio, fresh) = optimal_settings(freq_hz, MPX_SAMPLE_RATE);
                        demod_config = fresh;
                        if configure_sdr(&mut sdr, &radio, gain_db).is_ok() {
                            demod = DemodPipeline::new(demod_config, mono_only, deemphasis_us);
                            if let Ok(mut r) = ring.lock() {
                                r.clear();
                            }
                            let _ = event_tx.send(AppEvent::SdrConnected {
                                freq_hz,
                                gain_tenths: gain_db,
                            });
                        }
                    }
                    SdrCommand::SetMono(mono) => {
                        mono_only = mono;
                        demod = DemodPipeline::new(demod_config, mono_only, deemphasis_us);
                        resampler.reset(AUDIO_SAMPLE_RATE, device_rate);
                        if let Ok(mut r) = ring.lock() {
                            r.clear();
                        }
                    }
                    SdrCommand::SetPpm(new_ppm) => {
                        ppm = new_ppm;
                        let _ = sdr.set_freq_correction(ppm);
                    }
                    SdrCommand::SetDeemphasis(us) => {
                        deemphasis_us = us;
                        demod.set_deemphasis(deemphasis_us);
                        if let Ok(mut r) = ring.lock() {
                            r.clear();
                        }
                    }
                }
            }

            match sdr.read_sync(&mut buf) {
                Ok(n) if n >= DEFAULT_BUF_LENGTH => {
                    let frame = demod.process_iq(&buf);

                    let (audio_l, audio_r) = resampler.process(&frame.audio_left, &frame.audio_right);
                    {
                        let mut r = ring.lock().unwrap();
                        r.push_frame(&audio_l, &audio_r);
                    }

                    let (scope_src_l, scope_src_r) = if mono_only {
                        (&audio_l[..], &audio_r[..])
                    } else {
                        (&frame.scope_left[..], &frame.scope_right[..])
                    };
                    let (scope_l, scope_r) = decimate_scope_pair(scope_src_l, scope_src_r, 192);

                    let _ = event_tx.send(AppEvent::StereoData {
                        scope_left: scope_l,
                        scope_right: scope_r,
                        peak_l: frame.peak_l,
                        peak_r: frame.peak_r,
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
        thread::sleep(Duration::from_millis(300));
        if stop.load(Ordering::Relaxed) || app_shutdown.load(Ordering::Relaxed) {
            break;
        }
    }
}

/// ~200 ms of stereo audio at 48 kHz.
const MAX_RING_SAMPLES: usize = 9_600;

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

    pub fn clear(&mut self) {
        self.left.clear();
        self.right.clear();
        self.read_pos = 0;
    }

    pub fn push_frame(&mut self, left: &[f32], right: &[f32]) {
        self.left.extend_from_slice(left);
        self.right.extend_from_slice(right);
        if self.left.len() > MAX_RING_SAMPLES {
            let drop = self.left.len() - MAX_RING_SAMPLES;
            self.left.drain(..drop);
            self.right.drain(..drop);
            self.read_pos = self.read_pos.saturating_sub(drop);
        }
    }

    /// Read interleaved L/R into `out`. Returns number of stereo frames read.
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
        frames
    }
}

pub type SharedRing = Arc<Mutex<SampleRing>>;

pub fn new_shared_ring() -> SharedRing {
    Arc::new(Mutex::new(SampleRing::new()))
}
