//! FM demodulation ported from rtl-sdr-rs `simple_fm` (rtl_fm algorithm).

use num_complex::Complex;
use rtl_sdr_rs::RtlSdr;
use std::f64::consts::PI;

pub const MPX_SAMPLE_RATE: u32 = 192_000;
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

pub struct RadioConfig {
    pub capture_freq: u32,
    pub capture_rate: u32,
}

#[derive(Clone, Copy)]
pub struct DemodConfig {
    pub downsample: u32,
    pub rate_out: u32,
    pub rate_resample: u32,
}

pub fn optimal_settings(freq_hz: u32, mpx_rate: u32) -> (RadioConfig, DemodConfig) {
    let downsample = (1_000_000 / mpx_rate) + 1;
    let capture_rate = downsample * mpx_rate;
    let capture_freq = freq_hz + capture_rate / 4;
    (
        RadioConfig {
            capture_freq,
            capture_rate,
        },
        DemodConfig {
            downsample,
            rate_out: mpx_rate,
            rate_resample: AUDIO_SAMPLE_RATE,
        },
    )
}

pub fn configure_sdr(
    sdr: &mut RtlSdr,
    config: &RadioConfig,
    gain_db: i32,
) -> rtl_sdr_rs::error::Result<()> {
    let gain = if gain_db < 0 {
        rtl_sdr_rs::TunerGain::Auto
    } else {
        rtl_sdr_rs::TunerGain::Manual(gain_db)
    };
    sdr.set_tuner_gain(gain)?;
    sdr.set_bias_tee(false)?;
    sdr.reset_buffer()?;
    sdr.set_center_freq(config.capture_freq)?;
    sdr.set_sample_rate(config.capture_rate)?;
    Ok(())
}

/// rtl_fm-compatible FM receiver: IQ bytes -> MPX (170 kHz) and/or audio (48 kHz mono).
pub struct RtlFmReceiver {
    config: DemodConfig,
    prev_index: usize,
    now_lpr: i32,
    prev_lpr_index: i32,
    lp_now: Complex<i32>,
    demod_pre: Complex<i32>,
}

impl RtlFmReceiver {
    pub fn new(config: DemodConfig) -> Self {
        Self {
            config,
            prev_index: 0,
            now_lpr: 0,
            prev_lpr_index: 0,
            lp_now: Complex::new(0, 0),
            demod_pre: Complex::new(0, 0),
        }
    }

    /// Full MPX discriminator output at 170 kHz (for stereo decode).
    pub fn process_mpx(&mut self, iq: &[u8]) -> Vec<f32> {
        let lowpassed = self.downsample_complex(iq);
        let demodulated = self.fm_demod(lowpassed);
        demodulated
            .into_iter()
            .map(|s| s as f32 / 16384.0)
            .collect()
    }

    /// Mono audio at 48 kHz via rtl_fm decimation (proven path).
    pub fn process_mono_audio(&mut self, iq: &[u8]) -> Vec<f32> {
        let lowpassed = self.downsample_complex(iq);
        let demodulated = self.fm_demod(lowpassed);
        let audio = self.decimate_audio(demodulated);
        audio
            .into_iter()
            .map(|s| s as f32 / 32768.0)
            .collect()
    }

    fn downsample_complex(&mut self, iq: &[u8]) -> Vec<Complex<i32>> {
        let mut buf = iq.to_vec();
        rotate_90(&mut buf);
        let signed: Vec<i16> = buf.iter().map(|&v| v as i16 - 127).collect();
        let complex = bytes_to_complex(&signed);
        self.low_pass_complex(complex)
    }

    fn low_pass_complex(&mut self, buf: Vec<Complex<i32>>) -> Vec<Complex<i32>> {
        let mut res = Vec::new();
        for sample in buf {
            self.lp_now += sample;
            self.prev_index += 1;
            if self.prev_index < self.config.downsample as usize {
                continue;
            }
            res.push(self.lp_now);
            self.lp_now = Complex::new(0, 0);
            self.prev_index = 0;
        }
        res
    }

    fn fm_demod(&mut self, buf: Vec<Complex<i32>>) -> Vec<i32> {
        if buf.is_empty() {
            return Vec::new();
        }
        let mut result = Vec::with_capacity(buf.len());
        let mut pcm = polar_discriminant(buf[0], self.demod_pre);
        result.push(pcm);
        for i in 1..buf.len() {
            pcm = polar_discriminant_fast(buf[i], buf[i - 1]);
            result.push(pcm);
        }
        self.demod_pre = *buf.last().unwrap();
        result
    }

    fn decimate_audio(&mut self, buf: Vec<i32>) -> Vec<i16> {
        let mut result = Vec::new();
        let slow = self.config.rate_resample;
        let fast = self.config.rate_out;
        let mut i = 0;
        while i < buf.len() {
            self.now_lpr += buf[i];
            i += 1;
            self.prev_lpr_index += slow as i32;
            if self.prev_lpr_index < fast as i32 {
                continue;
            }
            result.push((self.now_lpr / ((fast / slow) as i32)) as i16);
            self.prev_lpr_index -= fast as i32;
            self.now_lpr = 0;
        }
        result
    }
}

fn rotate_90(buf: &mut [u8]) {
    for i in (0..buf.len()).step_by(8) {
        if i + 7 >= buf.len() {
            break;
        }
        let tmp = 255u8.wrapping_sub(buf[i + 3]);
        buf[i + 3] = buf[i + 2];
        buf[i + 2] = tmp;
        buf[i + 4] = 255u8.wrapping_sub(buf[i + 4]);
        buf[i + 5] = 255u8.wrapping_sub(buf[i + 5]);
        let tmp = 255u8.wrapping_sub(buf[i + 6]);
        buf[i + 6] = buf[i + 7];
        buf[i + 7] = tmp;
    }
}

fn bytes_to_complex(buf: &[i16]) -> Vec<Complex<i32>> {
    buf.chunks_exact(2)
        .map(|w| Complex::new(w[0] as i32, w[1] as i32))
        .collect()
}

fn polar_discriminant(a: Complex<i32>, b: Complex<i32>) -> i32 {
    let c = a * b.conj();
    let angle = (c.im as f64).atan2(c.re as f64);
    (angle / PI * (1 << 14) as f64) as i32
}

fn polar_discriminant_fast(a: Complex<i32>, b: Complex<i32>) -> i32 {
    let c = a * b.conj();
    fast_atan2(c.im, c.re)
}

fn fast_atan2(y: i32, x: i32) -> i32 {
    let pi4 = 1 << 12;
    let pi34 = 3 * (1 << 12);
    if x == 0 && y == 0 {
        return 0;
    }
    let mut yabs = y;
    if yabs < 0 {
        yabs = -yabs;
    }
    let angle = if x >= 0 {
        pi4 - (pi4 as i64 * (x - yabs) as i64) as i32 / (x + yabs)
    } else {
        pi34 - (pi4 as i64 * (x + yabs) as i64) as i32 / (yabs - x)
    };
    if y < 0 { -angle } else { angle }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optimal_settings_matches_simple_fm() {
        let (radio, demod) = optimal_settings(94_900_000, 192_000);
        assert_eq!(demod.downsample, 6);
        assert_eq!(radio.capture_rate, 1_152_000);
        assert_eq!(radio.capture_freq, 94_900_000 + 1_152_000 / 4);
    }

    #[test]
    fn optimal_settings_170k_legacy() {
        let (radio, demod) = optimal_settings(94_900_000, 170_000);
        assert_eq!(demod.downsample, 6);
        assert_eq!(radio.capture_rate, 1_020_000);
    }
}
