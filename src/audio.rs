//! cpal stereo audio output.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfigRange,
};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};

use crate::sdr::SharedRing;

pub struct AudioOutput {
    _stream: Stream,
    pub device_name: String,
    pub sample_rate: u32,
}

pub fn pick_output_device(name_filter: Option<&str>) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    if let Some(filter) = name_filter {
        let devices: Vec<_> = host
            .output_devices()
            .map_err(|e| e.to_string())?
            .collect();
        for device in devices {
            if let Ok(name) = device.name() {
                if name.to_lowercase().contains(&filter.to_lowercase()) {
                    return Ok(device);
                }
            }
        }
        return Err(format!("No audio output device matching '{filter}'"));
    }
    host.default_output_device()
        .ok_or_else(|| "No default audio output device".to_string())
}

fn pick_sample_rate(device: &cpal::Device, target: u32) -> Result<u32, String> {
    let preferred = [target, 48_000, 96_000, 192_000, 44_100];
    let configs: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .collect();

    for rate in preferred {
        for cfg in &configs {
            if rate >= cfg.min_sample_rate().0 && rate <= cfg.max_sample_rate().0 {
                return Ok(rate);
            }
        }
    }

    device
        .default_output_config()
        .map(|c| c.sample_rate().0)
        .map_err(|e| e.to_string())
}

/// Lower rank = preferred format when multiple configs match the same rate.
fn format_rank(fmt: SampleFormat) -> Option<u8> {
    match fmt {
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::U16 => Some(2),
        SampleFormat::I32 => Some(3),
        SampleFormat::F64 => Some(4),
        SampleFormat::I8 => Some(5),
        SampleFormat::U8 => Some(6),
        _ => None,
    }
}

fn pick_output_config(device: &cpal::Device, rate: u32) -> Result<SupportedStreamConfigRange, String> {
    device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .filter(|c| rate >= c.min_sample_rate().0 && rate <= c.max_sample_rate().0)
        .filter_map(|c| format_rank(c.sample_format()).map(|rank| (rank, c)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, cfg)| cfg)
        .ok_or_else(|| "No supported config for chosen sample rate".to_string())
}

const INPUT_BUF_COMPACT_THRESHOLD: usize = 8192;

struct ResamplerState {
    resampler: SincFixedIn<f32>,
    input_buf: Vec<f32>,
    read_pos: usize,
    channels: usize,
}

impl ResamplerState {
    fn new(input_rate: u32, output_rate: u32, chunk_frames: usize) -> Self {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        let resampler = SincFixedIn::<f32>::new(
            output_rate as f64 / input_rate as f64,
            2.0,
            params,
            chunk_frames,
            2,
        )
        .expect("resampler init");
        Self {
            resampler,
            input_buf: Vec::new(),
            read_pos: 0,
            channels: 2,
        }
    }

    fn available_frames(&self) -> usize {
        (self.input_buf.len().saturating_sub(self.read_pos)) / self.channels
    }

    fn compact_if_needed(&mut self) {
        if self.read_pos >= INPUT_BUF_COMPACT_THRESHOLD {
            self.input_buf.drain(..self.read_pos);
            self.read_pos = 0;
        }
    }

    fn deinterleave_chunk(&self, chunk: usize) -> Vec<Vec<f32>> {
        let base = self.read_pos;
        (0..self.channels)
            .map(|ch| {
                (0..chunk)
                    .map(|i| self.input_buf[base + i * self.channels + ch])
                    .collect()
            })
            .collect()
    }

    fn process_available(&mut self, out_l: &mut Vec<f32>, out_r: &mut Vec<f32>) {
        loop {
            let chunk = self.resampler.input_frames_next();
            if self.available_frames() < chunk {
                break;
            }

            let input = self.deinterleave_chunk(chunk);
            self.read_pos += chunk * self.channels;

            if let Ok(out) = self.resampler.process(&input, None) {
                out_l.extend_from_slice(&out[0]);
                out_r.extend_from_slice(&out[1]);
            }

            self.compact_if_needed();
        }
    }

    fn feed(&mut self, left: &[f32], right: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let len = left.len().min(right.len());
        self.input_buf.reserve(len * self.channels);
        for i in 0..len {
            self.input_buf.push(left[i]);
            self.input_buf.push(right[i]);
        }

        let mut out_l = Vec::new();
        let mut out_r = Vec::new();
        self.process_available(&mut out_l, &mut out_r);
        (out_l, out_r)
    }

    /// Drain any buffered input, zero-padding the final partial chunk.
    fn flush(&mut self) -> (Vec<f32>, Vec<f32>) {
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();

        loop {
            let chunk = self.resampler.input_frames_next();
            let available = self.available_frames();
            if available == 0 {
                break;
            }
            if available < chunk {
                let pad_frames = chunk - available;
                self.input_buf
                    .extend(std::iter::repeat_n(0.0f32, pad_frames * self.channels));
            }

            let input = self.deinterleave_chunk(chunk);
            self.read_pos += chunk * self.channels;

            if let Ok(out) = self.resampler.process(&input, None) {
                out_l.extend_from_slice(&out[0]);
                out_r.extend_from_slice(&out[1]);
            }

            self.compact_if_needed();
        }

        self.input_buf.clear();
        self.read_pos = 0;
        (out_l, out_r)
    }
}

/// Resamples stereo L/R when device rate differs from demod output (48 kHz).
pub struct StereoResampler {
    inner: Option<ResamplerState>,
}

impl StereoResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        if input_rate == output_rate {
            Self { inner: None }
        } else {
            Self {
                inner: Some(ResamplerState::new(input_rate, output_rate, 256)),
            }
        }
    }

    pub fn reset(&mut self, input_rate: u32, output_rate: u32) {
        *self = Self::new(input_rate, output_rate);
    }

    pub fn process(&mut self, left: &[f32], right: &[f32]) -> (Vec<f32>, Vec<f32>) {
        match self.inner.as_mut() {
            Some(r) => r.feed(left, right),
            None => (left.to_vec(), right.to_vec()),
        }
    }

    /// Flush resampler delay / partial input (call at end of a stream).
    pub fn flush(&mut self) -> (Vec<f32>, Vec<f32>) {
        match self.inner.as_mut() {
            Some(r) => r.flush(),
            None => (Vec::new(), Vec::new()),
        }
    }
}

pub fn start_audio(
    device_filter: Option<&str>,
    target_rate: u32,
    ring: SharedRing,
) -> Result<AudioOutput, String> {
    let device = pick_output_device(device_filter)?;
    let device_name = device.name().unwrap_or_else(|_| "unknown".into());
    let negotiated = pick_sample_rate(&device, target_rate)?;

    if negotiated < target_rate {
        eprintln!(
            "Warning: device supports max {negotiated} Hz (requested {target_rate} Hz). \
             Playback will be resampled."
        );
    }

    let stream_config_range =
        pick_output_config(&device, negotiated)?.with_sample_rate(cpal::SampleRate(negotiated));

    let sample_format = stream_config_range.sample_format();
    let mut stream_config: StreamConfig = stream_config_range.config();
    if let SupportedBufferSize::Range { min, max } = stream_config_range.buffer_size() {
        let size = 1024u32.clamp(*min, *max);
        stream_config.buffer_size = BufferSize::Fixed(size);
    }
    let channels = stream_config.channels as usize;

    let ring_cb = ring.clone();

    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, ring_cb, channels)?,
        SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, ring_cb, channels)?,
        SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, ring_cb, channels)?,
        SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, ring_cb, channels)?,
        SampleFormat::F64 => build_stream::<f64>(&device, &stream_config, ring_cb, channels)?,
        SampleFormat::I8 => build_stream::<i8>(&device, &stream_config, ring_cb, channels)?,
        SampleFormat::U8 => build_stream::<u8>(&device, &stream_config, ring_cb, channels)?,
        other => {
            return Err(format!(
                "Unsupported sample format: {other:?}. Try -a <device>."
            ))
        }
    };

    stream.play().map_err(|e| e.to_string())?;

    eprintln!("Audio output: {device_name} @ {negotiated} Hz ({sample_format:?})");

    Ok(AudioOutput {
        _stream: stream,
        device_name,
        sample_rate: negotiated,
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    ring: SharedRing,
    channels: usize,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32>,
{
    let mut scratch = vec![0.0f32; 4096];
    let mut hold_l = 0.0f32;
    let mut hold_r = 0.0f32;
    let err_fn = |e| eprintln!("Audio stream error: {e}");

    device
        .build_output_stream(
            config,
            move |out: &mut [T], _| {
                let frames = out.len() / channels;
                let stereo_needed = frames * 2;
                if scratch.len() < stereo_needed {
                    scratch.resize(stereo_needed, 0.0);
                }

                let got = {
                    let mut ring = ring.lock().unwrap();
                    ring.read_interleaved(&mut scratch[..stereo_needed])
                };

                for frame in 0..frames {
                    let l = if frame < got {
                        scratch[frame * 2]
                    } else {
                        hold_l
                    };
                    let r = if frame < got {
                        scratch[frame * 2 + 1]
                    } else {
                        hold_r
                    };
                    if frame < got {
                        hold_l = l;
                        hold_r = r;
                    }
                }

                for (i, sample) in out.iter_mut().enumerate() {
                    let ch = i % channels;
                    let src = if ch == 0 {
                        if i / channels < got {
                            scratch[(i / channels) * 2]
                        } else {
                            hold_l
                        }
                    } else if ch == 1 {
                        if i / channels < got {
                            scratch[(i / channels) * 2 + 1]
                        } else {
                            hold_r
                        }
                    } else {
                        0.0
                    };
                    *sample = T::from_sample(src);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod resampler_tests {
    use super::StereoResampler;
    use std::time::Instant;

    #[test]
    fn resample_large_buffer_completes_quickly() {
        let input_rate = 44_100;
        let output_rate = 48_000;
        let frames = 500_000usize;
        let left: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.001).sin()).collect();
        let right: Vec<f32> = (0..frames).map(|i| (i as f32 * 0.002).cos()).collect();

        let mut resampler = StereoResampler::new(input_rate, output_rate);
        let start = Instant::now();
        let (out_l, out_r) = resampler.process(&left, &right);
        let (tail_l, tail_r) = resampler.flush();
        let elapsed = start.elapsed();

        assert!(!out_l.is_empty());
        assert_eq!(out_l.len(), out_r.len());
        assert_eq!(tail_l.len(), tail_r.len());
        let total_out = out_l.len() + tail_l.len();
        let expected = (frames as f64 * output_rate as f64 / input_rate as f64).round() as usize;
        assert!(
            total_out.abs_diff(expected) < 2048,
            "expected ~{expected} output frames, got {total_out}"
        );
        assert!(
            elapsed.as_secs() < 5,
            "resampling 500k frames took {:?} (regression: O(n²) hang)",
            elapsed
        );
    }
}
