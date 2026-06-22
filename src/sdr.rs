//! SDR device discovery, wait loop, and IQ capture thread (RTL-SDR and Airspy).

use crate::app::AppEvent;
use crate::audio::StereoResampler;
use crate::demod::{
    centered_iq_settings_with_rates, configure_sdr, optimal_settings, DemodConfig, DemodPipeline,
    RadioConfig, AUDIO_SAMPLE_RATE, MPX_SAMPLE_RATE,
};
use crate::display::decimate_scope_pair;
use rs_spy::{Airspy, IqConverter, RECOMMENDED_BUFFER_SIZE};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SdrKind {
    Rtl,
    Airspy,
    AirspyHf,
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

fn rtl_count() -> usize {
    RtlSdr::get_device_count().unwrap_or(0)
}

fn airspy_count() -> usize {
    Airspy::list_devices().map(|d| d.len()).unwrap_or(0)
}

fn airspy_hf_count() -> usize {
    crate::airspyhf::list_devices()
}

fn device_count() -> usize {
    rtl_count() + airspy_count() + airspy_hf_count()
}

fn detect_kind() -> Option<SdrKind> {
    if rtl_count() > 0 {
        Some(SdrKind::Rtl)
    } else if airspy_count() > 0 {
        Some(SdrKind::Airspy)
    } else if airspy_hf_count() > 0 {
        Some(SdrKind::AirspyHf)
    } else {
        None
    }
}

fn device_summary() -> String {
    let rtl = rtl_count();
    let airspy = airspy_count();
    let hf = airspy_hf_count();
    let mut parts = Vec::new();
    if rtl > 0 {
        parts.push(format!("{rtl} RTL-SDR"));
    }
    if airspy > 0 {
        parts.push(format!("{airspy} Airspy R2/Mini"));
    }
    if hf > 0 {
        parts.push(format!("{hf} Airspy HF+"));
    }
    if parts.is_empty() {
        "no devices".into()
    } else {
        parts.join(", ")
    }
}

pub fn wait_for_device() -> Result<(), String> {
    let frames = ['|', '/', '-', '\\'];
    let mut i = 0usize;
    loop {
        if device_count() > 0 {
            eprintln!("\rSDR detected ({})          ", device_summary());
            // Enumeration briefly opens the dongle; give USB a moment to release
            // before audio init and the real capture open.
            thread::sleep(Duration::from_millis(300));
            return Ok(());
        }
        eprint!("\rWaiting for SDR (RTL-SDR, Airspy, or Airspy HF+)... {}  ", frames[i % frames.len()]);
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

fn format_airspy_error(e: &rs_spy::Error) -> String {
    let msg = format!("{e}");
    if msg.to_ascii_lowercase().contains("busy") {
        format!(
            "{msg}\n\n\
             Another process may still have the Airspy open (e.g. a previous run):\n\
               pkill -f oscilloscope-me\n\
             Unplug/replug the device, then try again."
        )
    } else {
        msg
    }
}

fn airspy_gain_preset(gain_db: i32) -> u8 {
    if gain_db < 0 {
        // Moderate linearity preset for local FM; sensitivity overloads strong signals.
        8
    } else {
        (10 + gain_db / 20).clamp(0, 21) as u8
    }
}

fn configure_airspy(airspy: &Airspy, radio: &RadioConfig, gain_db: i32) -> Result<(), String> {
    airspy
        .set_sample_rate_for_iq(radio.capture_rate)
        .map_err(|e| format_airspy_error(&e))?;
    airspy
        .set_freq(radio.capture_freq)
        .map_err(|e| format_airspy_error(&e))?;
    let preset = airspy_gain_preset(gain_db);
    airspy
        .set_linearity_gain(preset)
        .map_err(|e| format_airspy_error(&e))?;
    let _ = airspy.set_rf_bias(false);
    airspy.start_rx().map_err(|e| format_airspy_error(&e))
}

fn airspy_iq_settings(freq_hz: u32, airspy: &Airspy) -> (RadioConfig, DemodConfig) {
    let supported = airspy.supported_sample_rates().unwrap_or_default();
    centered_iq_settings_with_rates(freq_hz, MPX_SAMPLE_RATE, &supported)
}

fn airspy_raw_to_rtl_iq(raw: &[u8], converter: &mut IqConverter, scratch: &mut Vec<f32>) -> Vec<u8> {
    scratch.clear();
    scratch.reserve(raw.len() / 2);
    for chunk in raw.chunks_exact(2) {
        let sample = u16::from_le_bytes([chunk[0], chunk[1]]) as f32;
        scratch.push(sample / 32768.0);
    }
    if scratch.len() < 4 {
        return Vec::new();
    }
    converter.process(scratch);
    scratch
        .iter()
        .map(|&s| ((s * 127.0).clamp(-127.0, 127.0) + 127.0) as u8)
        .collect()
}

struct CaptureState {
    freq_hz: u32,
    gain_db: i32,
    ppm: i32,
    mono_only: bool,
    deemphasis_us: u8,
    demod_config: DemodConfig,
    demod: DemodPipeline,
    resampler: StereoResampler,
}

impl CaptureState {
    fn new(
        freq_hz: u32,
        ppm: i32,
        gain_db: i32,
        mono_only: bool,
        deemphasis_us: u8,
        device_rate: u32,
    ) -> Self {
        let (_, demod_config) = optimal_settings(freq_hz, MPX_SAMPLE_RATE);
        Self {
            freq_hz,
            gain_db,
            ppm,
            mono_only,
            deemphasis_us,
            demod_config,
            demod: DemodPipeline::new(demod_config, mono_only, deemphasis_us),
            resampler: StereoResampler::new(AUDIO_SAMPLE_RATE, device_rate),
        }
    }

    fn reset_demod(&mut self) {
        self.demod = DemodPipeline::new(self.demod_config, self.mono_only, self.deemphasis_us);
    }

    fn emit_frame(
        &mut self,
        iq: &[u8],
        mono_only: bool,
        ring: &SharedRing,
        event_tx: &crossbeam_channel::Sender<AppEvent>,
    ) {
        let frame = self.demod.process_iq(iq);
        let (audio_l, audio_r) = self.resampler.process(&frame.audio_left, &frame.audio_right);
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
}

fn on_reconfigured(
    state: &mut CaptureState,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
) {
    state.reset_demod();
    if let Ok(mut r) = ring.lock() {
        r.clear();
    }
    let _ = event_tx.send(AppEvent::SdrConnected {
        freq_hz: state.freq_hz,
        gain_tenths: state.gain_db,
    });
}

fn handle_rtl_commands(
    cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    state: &mut CaptureState,
    sdr: &mut RtlSdr,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    device_rate: u32,
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            SdrCommand::SetFreq(new_freq) => {
                state.freq_hz = new_freq;
                let (radio, fresh) = optimal_settings(state.freq_hz, MPX_SAMPLE_RATE);
                state.demod_config = fresh;
                let ok = sdr.set_center_freq(radio.capture_freq).is_ok();
                if ok {
                    let _ = sdr.reset_buffer();
                }
                if ok {
                    on_reconfigured(state, ring, event_tx);
                }
            }
            SdrCommand::SetGain(new_gain) => {
                state.gain_db = new_gain;
                let (radio, fresh) = optimal_settings(state.freq_hz, MPX_SAMPLE_RATE);
                state.demod_config = fresh;
                let ok = configure_sdr(sdr, &radio, state.gain_db).is_ok();
                if ok {
                    on_reconfigured(state, ring, event_tx);
                }
            }
            SdrCommand::SetMono(mono) => {
                state.mono_only = mono;
                state.reset_demod();
                state.resampler.reset(AUDIO_SAMPLE_RATE, device_rate);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            }
            SdrCommand::SetPpm(new_ppm) => {
                state.ppm = new_ppm;
                let _ = sdr.set_freq_correction(state.ppm);
            }
            SdrCommand::SetDeemphasis(us) => {
                state.deemphasis_us = us;
                state.demod.set_deemphasis(state.deemphasis_us);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            }
        }
    }
}

fn handle_airspy_commands(
    cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    state: &mut CaptureState,
    airspy: &Airspy,
    converter: &mut IqConverter,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    device_rate: u32,
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            SdrCommand::SetFreq(new_freq) => {
                state.freq_hz = new_freq;
                let (radio, fresh) = airspy_iq_settings(state.freq_hz, airspy);
                state.demod_config = fresh;
                let ok = airspy.set_freq(radio.capture_freq).is_ok();
                if ok {
                    converter.reset();
                }
                if ok {
                    on_reconfigured(state, ring, event_tx);
                }
            }
            SdrCommand::SetGain(new_gain) => {
                state.gain_db = new_gain;
                let (_, fresh) = airspy_iq_settings(state.freq_hz, airspy);
                state.demod_config = fresh;
                let preset = airspy_gain_preset(state.gain_db);
                let ok = airspy.set_linearity_gain(preset).is_ok();
                if ok {
                    on_reconfigured(state, ring, event_tx);
                }
            }
            SdrCommand::SetMono(mono) => {
                state.mono_only = mono;
                state.reset_demod();
                state.resampler.reset(AUDIO_SAMPLE_RATE, device_rate);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            }
            SdrCommand::SetPpm(new_ppm) => {
                state.ppm = new_ppm;
            }
            SdrCommand::SetDeemphasis(us) => {
                state.deemphasis_us = us;
                state.demod.set_deemphasis(state.deemphasis_us);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            }
        }
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
    ppm: i32,
    gain_db: i32,
    mono_only: bool,
    deemphasis_us: u8,
    device_rate: u32,
    ring: SharedRing,
    event_tx: crossbeam_channel::Sender<AppEvent>,
    cmd_rx: crossbeam_channel::Receiver<SdrCommand>,
    stop: Arc<AtomicBool>,
    app_shutdown: Arc<AtomicBool>,
) {
    let mut state = CaptureState::new(freq_hz, ppm, gain_db, mono_only, deemphasis_us, device_rate);

    loop {
        if stop.load(Ordering::Relaxed) || app_shutdown.load(Ordering::Relaxed) {
            break;
        }

        let Some(kind) = detect_kind() else {
            let _ = event_tx.send(AppEvent::SdrDisconnected(
                "No SDR detected — waiting to reconnect…".into(),
            ));
            thread::sleep(Duration::from_secs(1));
            continue;
        };

        match kind {
            SdrKind::Rtl => {
                if !capture_rtl(
                    &mut state,
                    &ring,
                    &event_tx,
                    &cmd_rx,
                    device_rate,
                    stop.clone(),
                    app_shutdown.clone(),
                ) {
                    break;
                }
            }
            SdrKind::Airspy => {
                if !capture_airspy(
                    &mut state,
                    &ring,
                    &event_tx,
                    &cmd_rx,
                    device_rate,
                    stop.clone(),
                    app_shutdown.clone(),
                ) {
                    break;
                }
            }
            SdrKind::AirspyHf => {
                if !capture_airspy_hf(
                    &mut state,
                    &ring,
                    &event_tx,
                    &cmd_rx,
                    device_rate,
                    stop.clone(),
                    app_shutdown.clone(),
                ) {
                    break;
                }
            }
        }

        thread::sleep(Duration::from_millis(300));
    }
}

fn capture_rtl(
    state: &mut CaptureState,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    device_rate: u32,
    stop: Arc<AtomicBool>,
    app_shutdown: Arc<AtomicBool>,
) -> bool {
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
            return true;
        }
    };

    if state.ppm != 0 {
        let _ = sdr.set_freq_correction(state.ppm);
    }

    let (radio, demod_config) = optimal_settings(state.freq_hz, MPX_SAMPLE_RATE);
    state.demod_config = demod_config;
    if let Err(e) = configure_sdr(&mut sdr, &radio, state.gain_db) {
        let _ = event_tx.send(AppEvent::SdrDisconnected(format_sdr_error(&e)));
        thread::sleep(Duration::from_secs(1));
        return true;
    }

    let _ = event_tx.send(AppEvent::SdrConnected {
        freq_hz: state.freq_hz,
        gain_tenths: state.gain_db,
    });
    state.reset_demod();

    let mut buf = vec![0u8; DEFAULT_BUF_LENGTH];

    while !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed) {
        handle_rtl_commands(cmd_rx, state, &mut sdr, ring, event_tx, device_rate);

        match sdr.read_sync(&mut buf) {
            Ok(n) if n >= DEFAULT_BUF_LENGTH => {
                state.emit_frame(&buf, state.mono_only, ring, event_tx);
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
    !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed)
}

fn capture_airspy(
    state: &mut CaptureState,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    device_rate: u32,
    stop: Arc<AtomicBool>,
    app_shutdown: Arc<AtomicBool>,
) -> bool {
    let airspy = match Airspy::open_first() {
        Ok(s) => s,
        Err(e) => {
            let busy = format!("{e}").to_ascii_lowercase().contains("busy");
            let _ = event_tx.send(AppEvent::SdrDisconnected(format_airspy_error(&e)));
            thread::sleep(if busy {
                Duration::from_secs(2)
            } else {
                Duration::from_secs(1)
            });
            return true;
        }
    };

    let (radio, demod_config) = airspy_iq_settings(state.freq_hz, &airspy);
    state.demod_config = demod_config;
    if let Err(msg) = configure_airspy(&airspy, &radio, state.gain_db) {
        let _ = event_tx.send(AppEvent::SdrDisconnected(msg));
        thread::sleep(Duration::from_secs(1));
        return true;
    }

    let _ = event_tx.send(AppEvent::SdrConnected {
        freq_hz: state.freq_hz,
        gain_tenths: state.gain_db,
    });
    state.reset_demod();

    let mut raw_buf = vec![0u8; RECOMMENDED_BUFFER_SIZE];
    let mut converter = IqConverter::new();
    let mut f32_scratch = Vec::new();

    while !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed) {
        handle_airspy_commands(
            cmd_rx,
            state,
            &airspy,
            &mut converter,
            ring,
            event_tx,
            device_rate,
        );

        match airspy.read_sync(&mut raw_buf) {
            Ok(n) if n >= 4096 => {
                let iq = airspy_raw_to_rtl_iq(&raw_buf[..n], &mut converter, &mut f32_scratch);
                if !iq.is_empty() {
                    state.emit_frame(&iq, state.mono_only, ring, event_tx);
                }
            }
            Ok(_) => {
                let _ = event_tx.send(AppEvent::SdrDisconnected(
                    "Short read from Airspy — device may have been unplugged.".into(),
                ));
                break;
            }
            Err(e) => {
                let _ = event_tx.send(AppEvent::SdrDisconnected(format_airspy_error(&e)));
                break;
            }
        }
    }

    let _ = airspy.stop_rx();
    !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed)
}

#[cfg(has_airspyhf)]
fn handle_airspy_hf_commands(
    cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    state: &mut CaptureState,
    hf: &crate::airspyhf::AirspyHf,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    device_rate: u32,
    supported_rates: &[u32],
) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            SdrCommand::SetFreq(new_freq) => {
                state.freq_hz = new_freq;
                let (_, fresh) =
                    crate::airspyhf::hf_settings_for_mpx(state.freq_hz, supported_rates);
                state.demod_config = fresh;
                if hf.set_freq(state.freq_hz).is_ok() {
                    on_reconfigured(state, ring, event_tx);
                }
            }
            SdrCommand::SetGain(new_gain) => {
                state.gain_db = new_gain;
                let (_, fresh) =
                    crate::airspyhf::hf_settings_for_mpx(state.freq_hz, supported_rates);
                state.demod_config = fresh;
                if hf.set_gain(state.gain_db).is_ok() {
                    on_reconfigured(state, ring, event_tx);
                }
            }
            SdrCommand::SetMono(mono) => {
                state.mono_only = mono;
                state.reset_demod();
                state.resampler.reset(AUDIO_SAMPLE_RATE, device_rate);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            }
            SdrCommand::SetPpm(_) => {}
            SdrCommand::SetDeemphasis(us) => {
                state.deemphasis_us = us;
                state.demod.set_deemphasis(state.deemphasis_us);
                if let Ok(mut r) = ring.lock() {
                    r.clear();
                }
            }
        }
    }
}

#[cfg(has_airspyhf)]
fn capture_airspy_hf(
    state: &mut CaptureState,
    ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    device_rate: u32,
    stop: Arc<AtomicBool>,
    app_shutdown: Arc<AtomicBool>,
) -> bool {
    let mut hf = match crate::airspyhf::AirspyHf::open_first() {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx.send(AppEvent::SdrDisconnected(e));
            thread::sleep(Duration::from_secs(1));
            return true;
        }
    };

    let supported: Vec<u32> = hf.supported_rates().to_vec();
    let (radio, demod_config) = crate::airspyhf::hf_settings_for_mpx(state.freq_hz, &supported);
    state.demod_config = demod_config;
    if let Err(msg) = hf.configure(radio.capture_freq, radio.capture_rate, state.gain_db) {
        let _ = event_tx.send(AppEvent::SdrDisconnected(msg));
        thread::sleep(Duration::from_secs(1));
        return true;
    }

    let (iq_tx, iq_rx) = crossbeam_channel::bounded::<Vec<u8>>(8);
    if let Err(msg) = hf.start(iq_tx) {
        let _ = event_tx.send(AppEvent::SdrDisconnected(msg));
        thread::sleep(Duration::from_secs(1));
        return true;
    }

    let _ = event_tx.send(AppEvent::SdrConnected {
        freq_hz: state.freq_hz,
        gain_tenths: state.gain_db,
    });
    state.reset_demod();

    while !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed) {
        handle_airspy_hf_commands(
            cmd_rx,
            state,
            &hf,
            ring,
            event_tx,
            device_rate,
            &supported,
        );

        match iq_rx.recv_timeout(Duration::from_millis(250)) {
            Ok(iq) if !iq.is_empty() => {
                state.emit_frame(&iq, state.mono_only, ring, event_tx);
            }
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let _ = event_tx.send(AppEvent::SdrDisconnected(
                    "Airspy HF+ stream ended — device may have been unplugged.".into(),
                ));
                break;
            }
        }
    }

    hf.stop();
    !stop.load(Ordering::Relaxed) && !app_shutdown.load(Ordering::Relaxed)
}

#[cfg(not(has_airspyhf))]
fn capture_airspy_hf(
    _state: &mut CaptureState,
    _ring: &SharedRing,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    _cmd_rx: &crossbeam_channel::Receiver<SdrCommand>,
    _device_rate: u32,
    _stop: Arc<AtomicBool>,
    _app_shutdown: Arc<AtomicBool>,
) -> bool {
    let _ = event_tx.send(AppEvent::SdrDisconnected(
        crate::airspyhf::missing_lib_message().into(),
    ));
    thread::sleep(Duration::from_secs(2));
    true
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
