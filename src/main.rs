mod app;
mod audio;
mod demod;
mod display;
mod file;
mod sdr;

use app::{AppEvent, AppState, GAIN_STEPS, ShutdownFlag, StereoSample};
use cpal::traits::{DeviceTrait, HostTrait};
use crate::demod::AUDIO_SAMPLE_RATE;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
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

    let input_rate = AUDIO_SAMPLE_RATE;

    let audio = audio::start_audio(
        cli.audio_device.as_deref(),
        cli.rate,
        ring.clone(),
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
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => shutdown.set(),
                    KeyCode::Char('+') | KeyCode::Char('=') if sdr_handle.is_some() => {
                        state.freq_hz = state.freq_hz.saturating_add(100_000);
                        tune_sdr(&mut sdr_handle, &mut state, &mut display_buf, &mut phosphor);
                    }
                    KeyCode::Char('-') if sdr_handle.is_some() => {
                        state.freq_hz = state.freq_hz.saturating_sub(100_000);
                        tune_sdr(&mut sdr_handle, &mut state, &mut display_buf, &mut phosphor);
                    }
                    KeyCode::Char('g') if sdr_handle.is_some() => {
                        state.gain_index = (state.gain_index + 1) % GAIN_STEPS.len();
                        state.gain_tenths = GAIN_STEPS[state.gain_index];
                        if let Some(h) = sdr_handle.as_ref() {
                            h.set_gain(state.gain_tenths);
                        }
                    }
                    KeyCode::Char('m') if sdr_handle.is_some() => {
                        state.mono_only = !state.mono_only;
                        reset_display(&mut display_buf, &mut phosphor);
                        if let Some(h) = sdr_handle.as_ref() {
                            h.set_mono(state.mono_only);
                        }
                    }
                    KeyCode::Char('l') if file_handle.is_some() => {
                        state.file_loop = !state.file_loop;
                        if let Some(h) = file_handle.as_ref() {
                            h.set_loop(state.file_loop);
                        }
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
