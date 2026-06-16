//! IQ sample processing and FM quadrature demodulation.

use num_complex::Complex;
use rtl_sdr_rs::RtlSdr;

/// Internal MPX sample rate used for stereo decoding.
pub const MPX_SAMPLE_RATE: u32 = 228_000;

pub struct RadioConfig {
    pub capture_freq: u32,
    pub capture_rate: u32,
    pub downsample: u32,
}

pub fn optimal_settings(freq_hz: u32, mpx_rate: u32) -> RadioConfig {
    let downsample = (1_000_000 / mpx_rate) + 1;
    let capture_rate = downsample * mpx_rate;
    let capture_freq = freq_hz + capture_rate / 4;
    RadioConfig {
        capture_freq,
        capture_rate,
        downsample,
    }
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

pub fn rotate_90(buf: &mut [u8]) {
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

pub fn bytes_to_complex(buf: &[u8]) -> Vec<Complex<f32>> {
    buf.chunks_exact(2)
        .map(|w| Complex::new(w[0] as f32 - 127.5, w[1] as f32 - 127.5))
        .collect()
}

pub struct FmDemodulator {
    downsample: usize,
    accum: Complex<f32>,
    accum_count: usize,
    prev_sample: Complex<f32>,
}

impl FmDemodulator {
    pub fn new(downsample: u32) -> Self {
        Self {
            downsample: downsample as usize,
            accum: Complex::new(0.0, 0.0),
            accum_count: 0,
            prev_sample: Complex::new(1.0, 0.0),
        }
    }

    pub fn process_iq(&mut self, iq: &[u8]) -> Vec<f32> {
        let mut buf = iq.to_vec();
        rotate_90(&mut buf);
        let complex = bytes_to_complex(&buf);
        self.downsample_and_demod(&complex)
    }

    fn downsample_and_demod(&mut self, samples: &[Complex<f32>]) -> Vec<f32> {
        let mut mpx = Vec::with_capacity(samples.len() / self.downsample + 1);
        for &sample in samples {
            self.accum += sample;
            self.accum_count += 1;
            if self.accum_count >= self.downsample {
                let avg = self.accum / self.accum_count as f32;
                let demod = self.quadrature_demod(avg);
                mpx.push(demod);
                self.accum = Complex::new(0.0, 0.0);
                self.accum_count = 0;
            }
        }
        mpx
    }

    fn quadrature_demod(&mut self, sample: Complex<f32>) -> f32 {
        let product = sample * self.prev_sample.conj();
        self.prev_sample = sample;
        product.im.atan2(product.re)
    }
}
