//! Terminal X/Y vectorscope using ratatui.

use crate::app::{AppState, StereoSample};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, Paragraph};
use ratatui::Frame;

const DISPLAY_POINTS: usize = 1024;

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

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let freq_mhz = state.freq_hz as f64 / 1_000_000.0;
    let mode = if state.is_stereo { "STEREO" } else { "MONO" };
    let sdr = if state.sdr_connected {
        "SDR OK"
    } else {
        "SDR --"
    };
    let status = if state.status_message.is_empty() {
        String::new()
    } else {
        format!(" │ {}", state.status_message.lines().next().unwrap_or(""))
    };
    let text = format!(
        " {freq_mhz:.1} MHz │ {sdr} │ {mode} │ {} @ {} Hz │ L {:+.0} dBFS │ R {:+.0} dBFS{status}",
        state.audio_device, state.audio_rate, state.peak_l, state.peak_r
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" oscilloscope-me ");
    let para = Paragraph::new(text).block(block);
    frame.render_widget(para, area);
}

fn draw_vectorscope(frame: &mut Frame, area: Rect, state: &AppState) {
    let points = normalize_points(&state.display_samples);
    let bounds = compute_bounds(&points);
    let data: Vec<(f64, f64)> = points
        .iter()
        .map(|p| (p.x as f64, p.y as f64))
        .collect();

    let datasets = vec![Dataset::default()
        .marker(Marker::Braille)
        .style(Style::default().fg(Color::Cyan))
        .graph_type(ratatui::widgets::GraphType::Scatter)
        .data(&data)];

    let x_bounds = [bounds.0, bounds.1];
    let y_bounds = [bounds.2, bounds.3];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" X/Y vectorscope (L → X, R → Y) "),
        )
        .x_axis(
            Axis::default()
                .bounds(x_bounds)
                .labels(vec![
                    Line::from("L"),
                    Line::from(""),
                    Line::from("R"),
                ]),
        )
        .y_axis(
            Axis::default()
                .bounds(y_bounds)
                .labels(vec![Line::from("Y"), Line::from(""), Line::from("")]),
        );

    frame.render_widget(chart, area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = " q quit │ + / - tune ±0.1 MHz │ g cycle gain ";
    let para = Paragraph::new(text);
    frame.render_widget(para, area);
}

fn normalize_points(samples: &[StereoSample]) -> Vec<StereoSample> {
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

fn compute_bounds(points: &[StereoSample]) -> (f64, f64, f64, f64) {
    if points.is_empty() {
        return (-1.0, 1.0, -1.0, 1.0);
    }
    let mut min_x = f32::MAX;
    let mut max_x = f32::MIN;
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for p in points {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    let pad = 0.1f64;
    let cx = ((min_x + max_x) as f64) / 2.0;
    let cy = ((min_y + max_y) as f64) / 2.0;
    let half = ((max_x - min_x).max(max_y - min_y) as f64 / 2.0).max(0.05);
    (
        cx - half - pad,
        cx + half + pad,
        cy - half - pad,
        cy + half + pad,
    )
}

pub fn downsample_for_display(
    left: &[f32],
    right: &[f32],
    buf: &mut Vec<StereoSample>,
    max_points: usize,
) {
    buf.clear();
    if left.is_empty() {
        return;
    }
    let len = left.len().min(right.len());
    let step = (len / max_points).max(1);
    for i in (0..len).step_by(step) {
        buf.push(StereoSample {
            x: left[i],
            y: right[i],
        });
    }
}
