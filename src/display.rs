//! Terminal X/Y vectorscope using ratatui.

use crate::app::{AppState, StereoSample};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Line, Points};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::Frame;

const DISPLAY_POINTS: usize = 512;
pub const TRACE_CAPACITY: usize = 2_048;
const MAX_DEVICE_NAME: usize = 20;
const SCOPE_HALF: f64 = 1.0;
const PHOSPHOR_GRID: usize = 128;
const PHOSPHOR_DECAY: f32 = 0.86;
const AUTOSCALE_FLOOR: f32 = 0.05;

/// Decimating pick for scope IPC (~128–256 points per SDR chunk).
pub fn decimate_scope_pair(left: &[f32], right: &[f32], target: usize) -> (Vec<f32>, Vec<f32>) {
    if left.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let len = left.len().min(right.len());
    let step = (len / target).max(1);
    let mut sl = Vec::with_capacity(target);
    let mut sr = Vec::with_capacity(target);
    let mut i = 0;
    while i < len && sl.len() < target {
        sl.push(left[i]);
        sr.push(right[i]);
        i += step;
    }
    (sl, sr)
}

/// Decaying hit grid for scope persistence (phosphor simulation).
pub struct Phosphor {
    cells: Vec<f32>,
}

impl Phosphor {
    pub fn new() -> Self {
        Self {
            cells: vec![0.0; PHOSPHOR_GRID * PHOSPHOR_GRID],
        }
    }

    pub fn clear(&mut self) {
        self.cells.fill(0.0);
    }

    pub fn splat(&mut self, left: &[f32], right: &[f32]) {
        if left.is_empty() {
            return;
        }
        let peak = scope_peak(left, right);
        let len = left.len().min(right.len());
        for i in 0..len {
            let (x, y) = normalize_xy(left[i], right[i], peak);
            stamp(self, x, y);
        }
    }

    pub fn decay(&mut self) {
        for c in &mut self.cells {
            *c *= PHOSPHOR_DECAY;
        }
    }

    fn coords(&self) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        for gy in 0..PHOSPHOR_GRID {
            for gx in 0..PHOSPHOR_GRID {
                if self.cells[gy * PHOSPHOR_GRID + gx] > 0.06 {
                    let x = gx as f64 / (PHOSPHOR_GRID - 1) as f64 * 2.0 * SCOPE_HALF - SCOPE_HALF;
                    let y = SCOPE_HALF - gy as f64 / (PHOSPHOR_GRID - 1) as f64 * 2.0 * SCOPE_HALF;
                    out.push((x, y));
                }
            }
        }
        out
    }
}

fn stamp(phosphor: &mut Phosphor, x: f32, y: f32) {
    if x.abs() > 1.05 || y.abs() > 1.05 {
        return;
    }
    let gx = ((x + 1.0) * 0.5 * (PHOSPHOR_GRID - 1) as f32).round() as usize;
    let gy = ((1.0 - y) * 0.5 * (PHOSPHOR_GRID - 1) as f32).round() as usize;
    if gx < PHOSPHOR_GRID && gy < PHOSPHOR_GRID {
        phosphor.cells[gy * PHOSPHOR_GRID + gx] = 1.0;
    }
}

fn scope_peak(left: &[f32], right: &[f32]) -> f32 {
    let peak = left
        .iter()
        .chain(right.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);
    peak.max(AUTOSCALE_FLOOR)
}

fn normalize_xy(l: f32, r: f32, peak: f32) -> (f32, f32) {
    (l / peak, r / peak)
}

pub fn draw(frame: &mut Frame, state: &AppState, display_buf: &[StereoSample], phosphor: &mut Phosphor) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(frame.area());

    draw_status(frame, chunks[0], state);
    draw_vectorscope(frame, chunks[1], display_buf, phosphor);
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
        "MONO"
    } else {
        "STEREO"
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

fn draw_vectorscope(frame: &mut Frame, area: Rect, samples: &[StereoSample], phosphor: &mut Phosphor) {
    phosphor.decay();
    let trace = decimate_for_render(samples);
    let peak = trace_peak(&trace);
    let phosphor_coords = phosphor.coords();
    let trace_coords: Vec<(f64, f64)> = trace
        .iter()
        .map(|p| {
            let (x, y) = normalize_xy(p.x, p.y, peak);
            (x as f64, y as f64)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" X/Y vectorscope (L -> X, R -> Y) ");

    Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds([-SCOPE_HALF, SCOPE_HALF])
        .y_bounds([-SCOPE_HALF, SCOPE_HALF])
        .paint(|ctx| {
            let grid = Color::DarkGray;
            let trace_color = Color::Cyan;
            let phosphor_color = Color::Blue;
            ctx.draw(&Line::new(-1.0, 0.0, 1.0, 0.0, grid));
            ctx.draw(&Line::new(0.0, -1.0, 0.0, 1.0, grid));

            if !phosphor_coords.is_empty() {
                ctx.draw(&Points {
                    coords: &phosphor_coords,
                    color: phosphor_color,
                });
            }

            for w in trace_coords.windows(2) {
                ctx.draw(&Line::new(
                    w[0].0, w[0].1, w[1].0, w[1].1, trace_color,
                ));
            }
        })
        .render(area, frame.buffer_mut());
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = " q quit | + / - tune | g gain | m mono/stereo ";
    let para = Paragraph::new(text);
    frame.render_widget(para, area);
}

fn trace_peak(samples: &[StereoSample]) -> f32 {
    let peak = samples
        .iter()
        .map(|p| p.x.abs().max(p.y.abs()))
        .fold(0.0f32, f32::max);
    peak.max(AUTOSCALE_FLOOR)
}

fn decimate_for_render(samples: &[StereoSample]) -> Vec<StereoSample> {
    if samples.is_empty() {
        return Vec::new();
    }
    let window = samples.len().min(TRACE_CAPACITY);
    let start = samples.len().saturating_sub(window);
    let tail = &samples[start..];
    let step = (tail.len() / DISPLAY_POINTS).max(1);
    tail.iter()
        .step_by(step)
        .take(DISPLAY_POINTS)
        .copied()
        .collect()
}

/// Append decimated scope L/R to the rolling trace buffer.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn decimate_scope_pair_targets_count() {
        let left: Vec<f32> = (0..10_000).map(|i| i as f32).collect();
        let right = left.clone();
        let (sl, sr) = decimate_scope_pair(&left, &right, 192);
        assert!(sl.len() <= 192);
        assert_eq!(sl.len(), sr.len());
        assert!(!sl.is_empty());
    }

    #[test]
    fn autoscale_maps_to_unit_range() {
        let peak = scope_peak(&[2.0], &[1.0]);
        let (x, y) = normalize_xy(2.0, 1.0, peak);
        assert!((x - 1.0).abs() < 1e-5);
        assert!((y - 0.5).abs() < 1e-5);
    }

    #[test]
    fn decimated_lissajous_is_not_diagonal() {
        let n = 4096usize;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / n as f32 * 2.0 * PI as f32;
            samples.push(StereoSample {
                x: t.sin(),
                y: t.cos(),
            });
        }

        let points = decimate_for_render(&samples);
        assert!(!points.is_empty());

        let peak = trace_peak(&points);
        let mut sum_xy = 0.0f64;
        let mut sum_xx = 0.0f64;
        let mut sum_yy = 0.0f64;
        for p in &points {
            let (x, y) = normalize_xy(p.x, p.y, peak);
            let x = x as f64;
            let y = y as f64;
            sum_xy += x * y;
            sum_xx += x * x;
            sum_yy += y * y;
        }
        let n = points.len() as f64;
        let corr = sum_xy / n;
        let var_x = sum_xx / n;
        let var_y = sum_yy / n;

        assert!(var_x > 0.1 && var_y > 0.1);
        assert!(
            corr.abs() < 0.5 * var_x.min(var_y),
            "sin/cos Lissajous should not collapse to diagonal; corr={corr}"
        );
    }

    #[test]
    fn phosphor_accumulates_circle() {
        let mut p = Phosphor::new();
        for i in 0..1000 {
            let t = i as f32 / 1000.0 * 2.0 * PI as f32;
            let l = vec![t.sin()];
            let r = vec![t.cos()];
            p.splat(&l, &r);
        }
        assert!(p.coords().len() > 50, "phosphor should retain a 2D pattern");
    }
}
