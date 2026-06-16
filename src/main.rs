mod app;
mod audio;
mod demod;
mod display;
mod sdr;

use app::{AppEvent, AppState, GAIN_STEPS, ShutdownFlag, StereoSample};
use cpal::traits::{DeviceTrait, HostTrait};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, stdout, Write};
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(name = "oscilloscope-me", about = "FM SDR receiver with terminal X/Y vectorscope")]
struct Cli {
    /// FM frequency in MHz
    #[arg(short, long)]
    freq: Option<f64>,

    /// Tuner gain in dB, or "auto"
    #[arg(short, long, default_value = "auto")]
    gain: String,

    /// Audio output device name substring
    #[arg(short, long)]
    audio_device: Option<String>,

    /// Target output sample rate in Hz
    #[arg(short, long, default_value_t = 192_000)]
    sample_rate: u32,

    /// Frequency correction PPM for TCXO
    #[arg(long, default_value_t = 0)]
    ppm: i32,

    /// Skip SDR wait (for testing UI without hardware)
    #[arg(long, hide = true)]
    no_sdr: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    print_diagnostics();

    if !cli.no_sdr {
        sdr::wait_for_device()?;
    }

    let freq_mhz = resolve_frequency(cli.freq)?;
    let freq_hz = (freq_mhz * 1_000_000.0).round() as u32;

    let mut state = AppState::new(freq_hz, cli.ppm);
    let initial_gain = parse_gain(&cli.gain)?;
    state.gain_index = GAIN_STEPS
        .iter()
        .position(|&g| g == initial_gain)
        .unwrap_or(0);
    let ring = sdr::new_shared_ring();
    let shutdown = ShutdownFlag::new();

    let audio = audio::start_audio(
        cli.audio_device.as_deref(),
        cli.sample_rate,
        ring.clone(),
    )?;
    state.audio_device = audio.device_name.clone();
    state.audio_rate = audio.sample_rate;

    let (event_tx, event_rx) = crossbeam_channel::unbounded();

    let sdr_shutdown = shutdown.handle();
    let mut sdr_handle = if cli.no_sdr {
        None
    } else {
        Some(sdr::start_capture(
            freq_hz,
            cli.ppm,
            GAIN_STEPS[state.gain_index],
            event_tx.clone(),
            sdr_shutdown,
        )?)
    };

    // ctrl-c handler
    let shutdown_sig = shutdown.handle();
    ctrlc_handler(shutdown_sig);

    let mut terminal = setup_terminal()?;
    let tick_rate = Duration::from_millis(33);
    let mut last_tick = Instant::now();
    let mut display_buf: Vec<StereoSample> = Vec::with_capacity(2048);

    loop {
        if shutdown.is_set() {
            break;
        }

        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                AppEvent::SdrConnected { freq_hz, .. } => {
                    state.sdr_connected = true;
                    state.freq_hz = freq_hz;
                    state.status_message.clear();
                }
                AppEvent::SdrDisconnected(msg) => {
                    state.sdr_connected = false;
                    state.status_message = msg;
                }
                AppEvent::StereoData {
                    left,
                    right,
                    is_stereo,
                    peak_l,
                    peak_r,
                } => {
                    state.is_stereo = is_stereo;
                    state.peak_l = peak_l;
                    state.peak_r = peak_r;
                    {
                        let mut r = ring.lock().unwrap();
                        r.push_frame(&left, &right);
                    }
                    display::downsample_for_display(&left, &right, &mut display_buf, 2048);
                    state.display_samples = display_buf.clone();
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            terminal.draw(|f| display::draw(f, &state))?;
            last_tick = Instant::now();
        }

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => shutdown.set(),
                    KeyCode::Char('+') | KeyCode::Char('=') => {
                        state.freq_hz = state.freq_hz.saturating_add(100_000);
                        restart_sdr(&mut sdr_handle, &state, &event_tx, &shutdown)?;
                    }
                    KeyCode::Char('-') => {
                        state.freq_hz = state.freq_hz.saturating_sub(100_000);
                        restart_sdr(&mut sdr_handle, &state, &event_tx, &shutdown)?;
                    }
                    KeyCode::Char('g') => {
                        state.gain_index = (state.gain_index + 1) % GAIN_STEPS.len();
                        restart_sdr(&mut sdr_handle, &state, &event_tx, &shutdown)?;
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(h) = sdr_handle {
        h.stop();
    }
    teardown_terminal(&mut terminal)?;
    Ok(())
}

fn restart_sdr(
    handle: &mut Option<sdr::SdrHandle>,
    state: &AppState,
    event_tx: &crossbeam_channel::Sender<AppEvent>,
    shutdown: &ShutdownFlag,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(h) = handle.take() {
        h.stop();
    }
    *handle = Some(sdr::start_capture(
        state.freq_hz,
        state.ppm,
        GAIN_STEPS[state.gain_index],
        event_tx.clone(),
        shutdown.handle(),
    )?);
    Ok(())
}

fn parse_gain(gain: &str) -> Result<i32, Box<dyn std::error::Error>> {
    if gain.eq_ignore_ascii_case("auto") {
        Ok(-1)
    } else {
        Ok(gain.parse()?)
    }
}

fn resolve_frequency(freq: Option<f64>) -> Result<f64, Box<dyn std::error::Error>> {
    if let Some(f) = freq {
        return Ok(f);
    }
    print!("FM frequency (MHz) [88.5]: ");
    stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(88.5)
    } else {
        Ok(trimmed.parse()?)
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
