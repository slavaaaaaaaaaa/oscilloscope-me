//! cpal stereo audio output with resampling to device rate.

use crate::demod::MPX_SAMPLE_RATE;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, Stream, StreamConfig};
use rubato::{Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction};
use std::sync::{Arc, Mutex};

use crate::sdr::SharedRing;

pub struct AudioOutput {
    _stream: Stream,
    pub device_name: String,
    pub sample_rate: u32,
    pub target_rate: u32,
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
    let preferred = [target, 192_000, 96_000, 48_000, 44_100];
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

pub struct ResamplerState {
    resampler: SincFixedIn<f32>,
    input_buf: Vec<f32>,
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
            channels: 2,
        }
    }

    fn push_interleaved(&mut self, samples: &mut [f32]) -> usize {
        self.input_buf.extend_from_slice(samples);
        let chunk = self.resampler.input_frames_next();
        let needed = chunk * self.channels;
        if self.input_buf.len() < needed {
            samples.fill(0.0);
            return samples.len();
        }

        let input: Vec<Vec<f32>> = (0..self.channels)
            .map(|ch| {
                self.input_buf
                    .iter()
                    .skip(ch)
                    .step_by(self.channels)
                    .take(chunk)
                    .copied()
                    .collect()
            })
            .collect();

        self.input_buf.drain(..needed);

        match self.resampler.process(&input, None) {
            Ok(out) => {
                let frames = out[0].len();
                for i in 0..frames {
                    if i * 2 + 1 < samples.len() {
                        samples[i * 2] = out[0][i];
                        samples[i * 2 + 1] = out[1][i];
                    }
                }
                let written = frames.min(samples.len() / 2) * 2;
                if written < samples.len() {
                    samples[written..].fill(0.0);
                }
                samples.len()
            }
            Err(_) => {
                samples.fill(0.0);
                samples.len()
            }
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
    let sample_rate = pick_sample_rate(&device, target_rate)?;
    let negotiated = sample_rate;

    if negotiated < target_rate {
        eprintln!(
            "Warning: device supports max {negotiated} Hz (requested {target_rate} Hz). \
             Scope fidelity may be reduced."
        );
    }

    let config = device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .find(|c| {
            negotiated >= c.min_sample_rate().0 && negotiated <= c.max_sample_rate().0
        })
        .ok_or_else(|| "No supported config for chosen sample rate".to_string())?
        .with_sample_rate(cpal::SampleRate(negotiated));

    let stream_config: StreamConfig = config.config();
    let sample_format = config.sample_format();
    let channels = stream_config.channels as usize;

    let needs_resample = negotiated != MPX_SAMPLE_RATE;
    let resampler = Arc::new(Mutex::new(if needs_resample {
        Some(ResamplerState::new(
            MPX_SAMPLE_RATE,
            negotiated,
            1024,
        ))
    } else {
        None
    }));

    let ring_cb = ring.clone();
    let resampler_cb = resampler.clone();

    let stream = match sample_format {
        SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, ring_cb, resampler_cb, channels)?,
        SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, ring_cb, resampler_cb, channels)?,
        SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, ring_cb, resampler_cb, channels)?,
        other => return Err(format!("Unsupported sample format: {other:?}")),
    };

    stream.play().map_err(|e| e.to_string())?;

    eprintln!("Audio: {device_name} @ {negotiated} Hz");

    Ok(AudioOutput {
        _stream: stream,
        device_name,
        sample_rate: negotiated,
        target_rate,
    })
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    ring: SharedRing,
    resampler: Arc<Mutex<Option<ResamplerState>>>,
    channels: usize,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32>,
{
    let mut scratch = vec![0.0f32; 4096];
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

                {
                    let mut ring = ring.lock().unwrap();
                    ring.read_interleaved(&mut scratch[..stereo_needed]);
                }

                let mut res = resampler.lock().unwrap();
                if let Some(r) = res.as_mut() {
                    r.push_interleaved(&mut scratch[..stereo_needed]);
                }

                for (i, sample) in out.iter_mut().enumerate() {
                    let ch = i % channels;
                    let src = if ch == 0 {
                        scratch[(i / channels) * 2]
                    } else if ch == 1 {
                        scratch[(i / channels) * 2 + 1]
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
