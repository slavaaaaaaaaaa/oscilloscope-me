//! Terminal X/Y vectorscope using ratatui.

use crate::app::{AppState, StereoSample};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;

const DISPLAY_POINTS: usize = 2048;
pub const TRACE_CAPACITY: usize = 16_384;
const MAX_DEVICE_NAME: usize = 20;
/// Fixed scope range; demod soft-limits peaks to ~0.85.
const SCOPE_HALF: f64 = 1.0;

pub fn draw(frame: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(frame.area());

    draw_status(frame, chunks[0], state);
    draw_vectorscope(frame, chunks[1], state);
    draw_help(frame, chunks[2]);
}

fn truncate_name(name: &str) -> String {
    if name.chars().count() <= MAX_DEVICE_NAME {
        name.to_string()
    } else {
        let end = name
            .char_indices()
            .map(|(i, _)| i)
            .nth(MAX_DEVICE_NAME - 1)
            .unwrap_or(name.len());
        format!("{}…", &name[..end])
    }
}

fn format_peak_db(peak: f32) -> String {
    if peak > 0.0 {
        "CLIP".to_string()
    } else {
        format!("{:.0}", peak)
    }
}

fn decode_mode_label(state: &AppState) -> &'static str {
    if state.mono_only {
        "FM-MONO"
    } else if state.is_stereo {
        "STEREO"
    } else {
        "STEREO (locking)"
    }
}

fn format_audio_rate(actual: u32, requested: u32) -> String {
    if requested != 0 && actual != requested {
        format!("{actual} Hz (wanted {requested})")
    } else {
        format!("{actual} Hz")
    }
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let freq_mhz = state.freq_hz as f64 / 1_000_000.0;
    let mode = decode_mode_label(state);
    let sdr = if state.sdr_connected {
        "SDR OK"
    } else {
        "SDR --"
    };
    let gain = state.gain_label();
    let device = truncate_name(&state.audio_device);
    let status = if state.status_message.is_empty() {
        String::new()
    } else {
        format!(
            " | {}",
            state.status_message.lines().next().unwrap_or("")
        )
    };
    let text = format!(
        " {freq_mhz:.1} MHz | {sdr} | {mode} | gain {gain} | {device} @ {} | L {} dBFS | R {} dBFS{status}",
        format_audio_rate(state.audio_rate, state.requested_rate),
        format_peak_db(state.peak_l),
        format_peak_db(state.peak_r),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" oscilloscope-me ");
    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn draw_vectorscope(frame: &mut Frame, area: Rect, state: &AppState) {
    let points = decimate_for_render(&state.display_samples);
    let data: Vec<(f64, f64)> = points
        .iter()
        .map(|p| (p.x as f64, p.y as f64))
        .collect();

    let bounds = scope_bounds();

    let datasets = vec![Dataset::default()
        .marker(Marker::Dot)
        .style(Style::default().fg(Color::Cyan))
        .graph_type(GraphType::Line)
        .data(&data)];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" X/Y vectorscope (L -> X, R -> Y) "),
        )
        .x_axis(
            Axis::default()
                .bounds([bounds.0, bounds.1])
                .labels(vec![
                    Line::from("-L"),
                    Line::from("0"),
                    Line::from("+L"),
                ]),
        )
        .y_axis(
            Axis::default()
                .bounds([bounds.2, bounds.3])
                .labels(vec![Line::from("+R"), Line::from("0"), Line::from("-R")]),
        );

    frame.render_widget(chart, area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = " q quit | + / - tune +/-0.1 MHz | g cycle gain ";
    let para = Paragraph::new(text);
    frame.render_widget(para, area);
}

fn scope_bounds() -> (f64, f64, f64, f64) {
    (-SCOPE_HALF, SCOPE_HALF, -SCOPE_HALF, SCOPE_HALF)
}

fn decimate_for_render(samples: &[StereoSample]) -> Vec<StereoSample> {
    if samples.is_empty() {
        return Vec::new();
    }
    let step = (samples.len() / DISPLAY_POINTS).max(1);
    samples
        .iter()
        .step_by(step)
        .take(DISPLAY_POINTS)
        .copied()
        .collect()
}

/// Append a demod chunk to the rolling trace buffer (keeps recent ~85 ms at 192 kHz).
pub fn append_for_display(left: &[f32], right: &[f32], buf: &mut Vec<StereoSample>) {
    if left.is_empty() {
        return;
    }
    let len = left.len().min(right.len());
    for i in 0..len {
        buf.push(StereoSample {
            x: left[i],
            y: right[i],
        });
    }
    if buf.len() > TRACE_CAPACITY {
        let drop = buf.len() - TRACE_CAPACITY;
        buf.drain(..drop);
    }
}
