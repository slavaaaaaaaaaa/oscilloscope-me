mod app;
mod audio;
mod controls;
mod demod;
mod display;
mod file;
mod sdr;

use app::{AppEvent, AppState, GAIN_STEPS, PromptKind, ShutdownFlag, StereoSample};
use audio::AudioControls;
use controls::{
    is_volume_down, is_volume_up, RepeatFilter, FILE_SEEK_SECS, TUNE_COARSE_MHZ, TUNE_STEP_MHZ,
    VOLUME_MAX, VOLUME_STEP,
};
use cpal::traits::{DeviceTrait, HostTrait};
use crate::demod::AUDIO_SAMPLE_RATE;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, stdout};
use std::time::{Duration, Instant};

const TICK_MS: u64 = 16;
const MAX_EVENTS_PER_FRAME: usize = 2;

#[derive(Parser, Debug)]
#[command(name = "oscilloscope-me", about = "FM SDR receiver with terminal X/Y vectorscope")]
struct Cli {
    /// FM frequency in MHz
    #[arg(short, long, default_value_t = 92.5)]
    freq: f64,

    /// Tuner gain in dB, or "auto"
    #[arg(short, long, default_value = "auto")]
    gain: String,

    /// Audio output device name substring
    #[arg(short, long)]
    audio_device: Option<String>,

    /// Target output sample rate in Hz
    #[arg(long = "sample-rate", short = 'r', default_value_t = 48_000, value_name = "HZ")]
    rate: u32,

    /// Frequency correction PPM for TCXO
    #[arg(long, default_value_t = 0)]
    ppm: i32,

    /// Start in mono decode mode
    #[arg(long)]
    mono: bool,

    /// Play oscilloscope music MP3 instead of SDR (L = X, R = Y)
    #[arg(long, value_name = "PATH")]
    file: Option<std::path::PathBuf>,

    /// Don't loop file playback (default: loop)
    #[arg(long)]
    no_loop: bool,

    /// Skip SDR wait (for testing UI without hardware)
    #[arg(long, hide = true)]
    no_sdr: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    print_diagnostics();

    let file_mode = cli.file.is_some();

    if !cli.no_sdr && !file_mode {
        sdr::wait_for_device()?;
    }

    let freq_hz = (cli.freq * 1_000_000.0).round() as u32;

    let mut state = AppState::new(freq_hz, cli.ppm);
    let initial_gain = parse_gain(&cli.gain)?;
    state.gain_index = GAIN_STEPS
        .iter()
        .position(|&g| g == initial_gain)
        .unwrap_or(0);
    state.gain_tenths = GAIN_STEPS[state.gain_index];
    state.mono_only = cli.mono;
    if file_mode {
        state.input_source = app::InputSource::File;
        state.file_loop = !cli.no_loop;
    }
    let ring = sdr::new_shared_ring();
    let shutdown = ShutdownFlag::new();
    let audio_controls = AudioControls::new();

    let input_rate = AUDIO_SAMPLE_RATE;

    let audio = audio::start_audio(
        cli.audio_device.as_deref(),
        cli.rate,
        ring.clone(),
        audio_controls.clone(),
    )?;
    state.audio_device = audio.device_name.clone();
    state.audio_rate = audio.sample_rate;
    state.requested_rate = cli.rate;

    if file_mode {
        eprintln!("File input mode (no SDR demod)");
    } else if input_rate != audio.sample_rate {
        eprintln!(
            "Demod output: {input_rate} Hz -> resampled to {} Hz for playback",
            audio.sample_rate
        );
    } else {
        eprintln!("Demod output: {input_rate} Hz (no resampling needed)");
    }

    let (event_tx, event_rx) = crossbeam_channel::unbounded();

    let sdr_shutdown = shutdown.handle();
    let mut sdr_handle = if cli.no_sdr || file_mode {
        None
    } else {
        Some(sdr::start_capture(
            freq_hz,
            cli.ppm,
            GAIN_STEPS[state.gain_index],
            state.mono_only,
            state.deemphasis_us,
            audio.sample_rate,
            ring.clone(),
            event_tx.clone(),
            sdr_shutdown,
        )?)
    };

    let mut decoded_track = None;
    let file_path = cli.file.clone();
    if let Some(path) = &file_path {
        let track = file::decode_mp3(path)?;
        eprintln!(
            "Loaded {} — {:.1}s @ {} Hz{}",
            path.display(),
            track.left.len() as f64 / track.sample_rate as f64,
            track.sample_rate,
            if state.file_loop { " (loop)" } else { "" }
        );
        if track.sample_rate != audio.sample_rate {
            eprintln!(
                "Will resample: {} Hz -> {} Hz during playback",
                track.sample_rate, audio.sample_rate
            );
        }
        decoded_track = Some(track);
    }

    let shutdown_sig = shutdown.handle();
    ctrlc_handler(shutdown_sig);

    let mut terminal = setup_terminal()?;
    if file_mode {
        state.status_message = "Preparing playback…".into();
    }

    let file_handle = if let (Some(path), Some(track)) = (file_path, decoded_track) {
        Some(file::start_playback(
            path,
            track,
            audio.sample_rate,
            state.file_loop,
            ring.clone(),
            event_tx.clone(),
            shutdown.handle(),
        )?)
    } else {
        None
    };

    let tick_rate = Duration::from_millis(TICK_MS);
    let mut last_tick = Instant::now();
    let mut display_buf: Vec<StereoSample> = Vec::with_capacity(display::TRACE_CAPACITY);
    let mut phosphor = display::Phosphor::new();
    let mut vol_filter = RepeatFilter::new();

    loop {
        if shutdown.is_set() {
            break;
        }

        let mut events_processed = 0usize;
        while events_processed < MAX_EVENTS_PER_FRAME {
            match event_rx.try_recv() {
                Ok(ev) => {
                    events_processed += 1;
                    match ev {
                        AppEvent::SdrConnected {
                            freq_hz,
                            gain_tenths,
                            ..
                        } => {
                            state.sdr_connected = true;
                            state.freq_hz = freq_hz;
                            state.gain_tenths = gain_tenths;
                            state.status_message.clear();
                        }
                        AppEvent::SdrDisconnected(msg) => {
                            state.sdr_connected = false;
                            state.status_message = msg;
                        }
                        AppEvent::FilePlaying {
                            path,
                            sample_rate,
                            loop_playback,
                        } => {
                            state.file_path = path;
                            state.file_loop = loop_playback;
                            state.status_message.clear();
                            eprintln!("Playing @ {sample_rate} Hz");
                        }
                        AppEvent::FileFinished => {
                            state.status_message = "Playback finished".into();
                        }
                        AppEvent::StereoData {
                            scope_left,
                            scope_right,
                            peak_l,
                            peak_r,
                        } => {
                            state.peak_l = peak_l;
                            state.peak_r = peak_r;
                            display::append_for_display(&scope_left, &scope_right, &mut display_buf);
                            phosphor.splat(&scope_left, &scope_right);
                        }
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }

        if last_tick.elapsed() >= tick_rate {
            terminal.draw(|f| display::draw(f, &state, &display_buf, &mut phosphor))?;
            last_tick = Instant::now();
        }

        let poll_ms = tick_rate
            .saturating_sub(last_tick.elapsed())
            .as_millis()
            .min(16) as u64;
        if event::poll(Duration::from_millis(poll_ms.max(1)))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if state.prompt.is_some() {
                    handle_prompt_key(
                        &key,
                        &mut state,
                        &mut sdr_handle,
                        &mut display_buf,
                        &mut phosphor,
                    );
                    continue;
                }
                if state.show_help
                    && matches!(key.code, KeyCode::Char('h') | KeyCode::Char('?'))
                {
                    state.show_help = false;
                    continue;
                }
                let vol_key = match key.code {
                    KeyCode::Char(c) if is_volume_up(c) || is_volume_down(c) => {
                        Some(c)
                    }
                    KeyCode::Char('-') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        Some('_')
                    }
                    _ => None,
                };
                if let Some(c) = vol_filter.filter(vol_key, Instant::now()) {
                    adjust_volume(&mut state, &audio_controls, c);
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => shutdown.set(),
                    KeyCode::Char('h') | KeyCode::Char('?') => {
                        state.show_help = !state.show_help;
                    }
                    KeyCode::Char(' ') => {
                        if sdr_handle.is_some() {
                            state.muted = !state.muted;
                            audio_controls.set_muted(state.muted);
                            state.status_message = if state.muted {
                                "muted".into()
                            } else {
                                "unmuted".into()
                            };
                        } else if file_handle.is_some() {
                            state.file_paused = !state.file_paused;
                            if let Some(h) = file_handle.as_ref() {
                                h.set_paused(state.file_paused);
                            }
                            state.status_message = if state.file_paused {
                                "paused".into()
                            } else {
                                "playing".into()
                            };
                        }
                    }
                    KeyCode::Up | KeyCode::Char('.') if sdr_handle.is_some() => {
                        tune_mhz(
                            TUNE_STEP_MHZ,
                            &mut sdr_handle,
                            &mut state,
                            &mut display_buf,
                            &mut phosphor,
                        );
                    }
                    KeyCode::Down | KeyCode::Char(',') if sdr_handle.is_some() => {
                        tune_mhz(
                            -TUNE_STEP_MHZ,
                            &mut sdr_handle,
                            &mut state,
                            &mut display_buf,
                            &mut phosphor,
                        );
                    }
                    KeyCode::Right | KeyCode::Char('>') if sdr_handle.is_some() => {
                        tune_mhz(
                            TUNE_COARSE_MHZ,
                            &mut sdr_handle,
                            &mut state,
                            &mut display_buf,
                            &mut phosphor,
                        );
                    }
                    KeyCode::Left | KeyCode::Char('<') if sdr_handle.is_some() => {
                        tune_mhz(
                            -TUNE_COARSE_MHZ,
                            &mut sdr_handle,
                            &mut state,
                            &mut display_buf,
                            &mut phosphor,
                        );
                    }
                    KeyCode::Left | KeyCode::Char('<') if file_handle.is_some() => {
                        if let Some(h) = file_handle.as_ref() {
                            h.seek_seconds(-FILE_SEEK_SECS);
                        }
                        state.file_paused = false;
                        state.status_message = format!("seek -{FILE_SEEK_SECS:.0}s");
                    }
                    KeyCode::Right | KeyCode::Char('>') if file_handle.is_some() => {
                        if let Some(h) = file_handle.as_ref() {
                            h.seek_seconds(FILE_SEEK_SECS);
                        }
                        state.file_paused = false;
                        state.status_message = format!("seek +{FILE_SEEK_SECS:.0}s");
                    }
                    KeyCode::Char('g') if sdr_handle.is_some() => {
                        start_prompt(&mut state, PromptKind::Gain, "Gain dB or 'auto'");
                    }
                    KeyCode::Char('f') if sdr_handle.is_some() => {
                        let label = format!("Frequency MHz [{:.1}]", mhz(state.freq_hz));
                        start_prompt(&mut state, PromptKind::Freq, &label);
                    }
                    KeyCode::Char('p') if sdr_handle.is_some() => {
                        let label = format!("ppm correction [{}]", state.ppm);
                        start_prompt(&mut state, PromptKind::Ppm, &label);
                    }
                    KeyCode::Char('m') if sdr_handle.is_some() => {
                        state.mono_only = !state.mono_only;
                        reset_display(&mut display_buf, &mut phosphor);
                        if let Some(h) = sdr_handle.as_ref() {
                            h.set_mono(state.mono_only);
                        }
                        state.status_message = if state.mono_only {
                            "mono".into()
                        } else {
                            "stereo".into()
                        };
                    }
                    KeyCode::Char('d') if sdr_handle.is_some() => {
                        state.cycle_deemphasis();
                        if let Some(h) = sdr_handle.as_ref() {
                            h.set_deemphasis(state.deemphasis_us);
                        }
                        state.status_message = format!("de-emph {}", state.deemphasis_label());
                    }
                    KeyCode::Char('l') if file_handle.is_some() => {
                        state.file_loop = !state.file_loop;
                        if let Some(h) = file_handle.as_ref() {
                            h.set_loop(state.file_loop);
                        }
                        state.status_message = if state.file_loop {
                            "loop on".into()
                        } else {
                            "loop off".into()
                        };
                    }
                    KeyCode::Char('r') if file_handle.is_some() => {
                        if let Some(h) = file_handle.as_ref() {
                            h.restart();
                        }
                        state.file_paused = false;
                        state.status_message = "restarted".into();
                    }
                    KeyCode::Char('o') => {
                        state.status_message =
                            "use --file <path> or restart without --file to switch source".into();
                    }
                    KeyCode::Char('f') if file_handle.is_some() => {
                        state.status_message =
                            "use --file to switch sources; restart without --file for SDR".into();
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(h) = sdr_handle {
        h.stop();
    }
    if let Some(h) = file_handle {
        h.stop();
    }
    teardown_terminal(&mut terminal)?;
    Ok(())
}

fn mhz(freq_hz: u32) -> f64 {
    freq_hz as f64 / 1_000_000.0
}

fn adjust_volume(state: &mut AppState, controls: &AudioControls, key: char) {
    if is_volume_up(key) {
        state.volume = (state.volume + VOLUME_STEP).min(VOLUME_MAX);
    } else if is_volume_down(key) {
        state.volume = (state.volume - VOLUME_STEP).max(0.0);
    }
    controls.set_volume(state.volume);
    state.status_message = format!("volume {:.2}", state.volume);
}

fn tune_mhz(
    delta_mhz: f64,
    handle: &mut Option<sdr::SdrHandle>,
    state: &mut AppState,
    display_buf: &mut Vec<StereoSample>,
    phosphor: &mut display::Phosphor,
) {
    let delta_hz = (delta_mhz * 1_000_000.0).round() as i64;
    state.freq_hz = ((state.freq_hz as i64) + delta_hz).max(0) as u32;
    tune_sdr(handle, state, display_buf, phosphor);
    state.status_message = format!("{:.1} MHz", mhz(state.freq_hz));
}

fn start_prompt(state: &mut AppState, kind: PromptKind, label: &str) {
    state.prompt = Some(kind);
    state.prompt_buf.clear();
    state.status_message = format!("{label}: ");
}

fn handle_prompt_key(
    key: &event::KeyEvent,
    state: &mut AppState,
    sdr_handle: &mut Option<sdr::SdrHandle>,
    display_buf: &mut Vec<StereoSample>,
    phosphor: &mut display::Phosphor,
) {
    match key.code {
        KeyCode::Esc => {
            state.prompt = None;
            state.prompt_buf.clear();
            state.status_message = "cancelled".into();
        }
        KeyCode::Enter => {
            let kind = state.prompt.take().unwrap();
            let input = state.prompt_buf.trim().to_string();
            state.prompt_buf.clear();
            if input.is_empty() {
                state.status_message = "cancelled".into();
                return;
            }
            match kind {
                PromptKind::Freq => match input.parse::<f64>() {
                    Ok(mhz) if mhz > 0.0 => {
                        state.freq_hz = (mhz * 1_000_000.0).round() as u32;
                        tune_sdr(sdr_handle, state, display_buf, phosphor);
                        state.status_message = format!("{mhz:.1} MHz");
                    }
                    _ => state.status_message = "bad frequency".into(),
                },
                PromptKind::Gain => match apply_gain_input(state, &input) {
                    Ok(()) => {
                        if let Some(h) = sdr_handle.as_ref() {
                            h.set_gain(state.gain_tenths);
                        }
                        state.status_message = format!("gain {}", state.gain_label());
                    }
                    Err(msg) => state.status_message = msg.into(),
                },
                PromptKind::Ppm => match input.parse::<i32>() {
                    Ok(ppm) => {
                        state.ppm = ppm;
                        if let Some(h) = sdr_handle.as_ref() {
                            h.set_ppm(ppm);
                        }
                        state.status_message = format!("ppm {ppm}");
                    }
                    Err(_) => state.status_message = "bad ppm".into(),
                },
            }
        }
        KeyCode::Backspace => {
            state.prompt_buf.pop();
            if let Some(kind) = state.prompt {
                let label = match kind {
                    PromptKind::Freq => format!("Frequency MHz [{:.1}]", mhz(state.freq_hz)),
                    PromptKind::Gain => format!("Gain dB or 'auto' [{}]", state.gain_label()),
                    PromptKind::Ppm => format!("ppm correction [{}]", state.ppm),
                };
                state.status_message = format!("{label}: {}", state.prompt_buf);
            }
        }
        KeyCode::Char(c) => {
            state.prompt_buf.push(c);
            if let Some(kind) = state.prompt {
                let label = match kind {
                    PromptKind::Freq => format!("Frequency MHz [{:.1}]", mhz(state.freq_hz)),
                    PromptKind::Gain => format!("Gain dB or 'auto' [{}]", state.gain_label()),
                    PromptKind::Ppm => format!("ppm correction [{}]", state.ppm),
                };
                state.status_message = format!("{label}: {}", state.prompt_buf);
            }
        }
        _ => {}
    }
}

fn apply_gain_input(state: &mut AppState, input: &str) -> Result<(), &'static str> {
    let lower = input.trim().to_lowercase();
    if lower == "auto" || lower == "a" {
        state.gain_index = 0;
        state.gain_tenths = GAIN_STEPS[0];
        return Ok(());
    }
    let db: f64 = input.trim().parse().map_err(|_| "bad gain")?;
    state.gain_tenths = (db * 10.0).round() as i32;
    state.gain_index = GAIN_STEPS
        .iter()
        .position(|&g| g == state.gain_tenths)
        .unwrap_or(1);
    Ok(())
}

fn tune_sdr(
    handle: &mut Option<sdr::SdrHandle>,
    state: &mut AppState,
    display_buf: &mut Vec<StereoSample>,
    phosphor: &mut display::Phosphor,
) {
    reset_display(display_buf, phosphor);
    if let Some(h) = handle.as_ref() {
        h.set_freq(state.freq_hz);
    }
}

fn reset_display(display_buf: &mut Vec<StereoSample>, phosphor: &mut display::Phosphor) {
    display_buf.clear();
    phosphor.clear();
}

fn parse_gain(gain: &str) -> Result<i32, Box<dyn std::error::Error>> {
    if gain.eq_ignore_ascii_case("auto") {
        Ok(-1)
    } else {
        Ok(gain.parse::<i32>()? * 10)
    }
}

fn print_diagnostics() {
    eprintln!("oscilloscope-me — FM SDR X/Y receiver");
    eprintln!("OS: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    let host = cpal::default_host();
    eprintln!("Audio host: {}", host.id().name());
    if let Ok(devices) = host.output_devices() {
        eprintln!("Audio outputs:");
        for d in devices {
            if let Ok(name) = d.name() {
                eprintln!("  - {name}");
            }
        }
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, io::Error> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    Terminal::new(backend)
}

fn teardown_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<(), io::Error> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn ctrlc_handler(shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let _ = ctrlc::set_handler(move || {
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}
