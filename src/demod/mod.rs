mod filters;
mod fm;
mod stereo;

pub use fm::{
    configure_sdr, optimal_settings, DemodConfig, MPX_SAMPLE_RATE, RadioConfig, RtlFmReceiver,
    AUDIO_SAMPLE_RATE,
};
pub use stereo::StereoDecoder;

pub struct DemodPipeline {
    fm: RtlFmReceiver,
    stereo: StereoDecoder,
    mono_only: bool,
    mono_deemph: filters::Deemphasis,
}

impl DemodPipeline {
    pub fn new(config: DemodConfig, mono_only: bool) -> Self {
        Self {
            fm: RtlFmReceiver::new(config),
            stereo: StereoDecoder::new(),
            mono_only,
            mono_deemph: filters::Deemphasis::us_broadcast(AUDIO_SAMPLE_RATE as f64),
        }
    }

    pub fn process_iq(&mut self, iq: &[u8]) -> StereoFrame {
        if self.mono_only {
            let mut mono = self.fm.process_mono_audio(iq);
            for s in mono.iter_mut() {
                *s = self.mono_deemph.process(*s as f64) as f32;
            }
            limit_samples(&mut mono);
            let peak_l = peak_dbfs(&mono);
            return StereoFrame {
                left: mono.clone(),
                right: mono,
                is_stereo: false,
                peak_l,
                peak_r: peak_l,
            };
        }

        let mpx = self.fm.process_mpx(iq);
        let (mut left, mut right) = self.stereo.process_mpx(&mpx);
        limit_stereo(&mut left, &mut right);
        let peak_l = peak_dbfs(&left);
        let peak_r = peak_dbfs(&right);
        StereoFrame {
            left,
            right,
            is_stereo: self.stereo.is_stereo(),
            peak_l,
            peak_r,
        }
    }
}

pub struct StereoFrame {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub is_stereo: bool,
    pub peak_l: f32,
    pub peak_r: f32,
}

pub fn peak_dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return -120.0;
    }
    let peak = samples
        .iter()
        .map(|s| s.abs())
        .fold(0.0f32, f32::max)
        .max(1e-9);
    20.0 * peak.log10()
}

/// Soft-limit samples to ~0.85 peak to avoid DAC clipping.
fn limit_samples(samples: &mut [f32]) {
    let peak = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    if peak > 1e-6 {
        let scale = 0.85 / peak;
        if scale < 1.0 {
            for s in samples.iter_mut() {
                *s *= scale;
            }
        }
    }
}

/// Soft-limit L/R to ~0.85 peak to avoid DAC clipping.
fn limit_stereo(left: &mut [f32], right: &mut [f32]) {
    let peak = left
        .iter()
        .chain(right.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);
    if peak > 1e-6 {
        let scale = 0.85 / peak;
        if scale < 1.0 {
            for s in left.iter_mut() {
                *s *= scale;
            }
            for s in right.iter_mut() {
                *s *= scale;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::filters::{Biquad, Deemphasis};

    #[test]
    fn lowpass_attenuates_high_frequency() {
        let sample_rate = 192_000.0;
        let mut lpf_low = Biquad::lowpass(sample_rate, 15_000.0, 0.707);
        let mut lpf_high = Biquad::lowpass(sample_rate, 15_000.0, 0.707);
        let mut low_out = 0.0;
        let mut high_out = 0.0;
        for i in 0..10_000 {
            let t = i as f64 / sample_rate;
            low_out = lpf_low.process((2.0 * std::f64::consts::PI * 1_000.0 * t).sin());
            high_out = lpf_high.process((2.0 * std::f64::consts::PI * 50_000.0 * t).sin());
        }
        assert!(low_out.abs() > 0.1);
        assert!(high_out.abs() < 0.1);
    }

    #[test]
    fn deemphasis_smoothes_steps() {
        let mut de = Deemphasis::us_broadcast(192_000.0);
        let first = de.process(1.0);
        let second = de.process(1.0);
        assert!(second > first);
        assert!(second < 1.0);
    }
}
