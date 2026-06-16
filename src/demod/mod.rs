mod filters;
mod fm;
mod stereo;

pub use fm::{
    configure_sdr, optimal_settings, DemodConfig, MPX_SAMPLE_RATE, RtlFmReceiver,
    StereoAudioDecimator, AUDIO_SAMPLE_RATE,
};
pub use stereo::StereoDecoder;

pub struct DemodPipeline {
    fm: RtlFmReceiver,
    stereo: StereoDecoder,
    decimator: StereoAudioDecimator,
    mono_only: bool,
    mono_deemph: filters::Deemphasis,
    deemph_l: filters::Deemphasis,
    deemph_r: filters::Deemphasis,
}

impl DemodPipeline {
    pub fn new(config: DemodConfig, mono_only: bool) -> Self {
        Self {
            fm: RtlFmReceiver::new(config),
            stereo: StereoDecoder::new(),
            decimator: StereoAudioDecimator::new(&config),
            mono_only,
            mono_deemph: filters::Deemphasis::us_broadcast(AUDIO_SAMPLE_RATE as f64),
            deemph_l: filters::Deemphasis::us_broadcast(AUDIO_SAMPLE_RATE as f64),
            deemph_r: filters::Deemphasis::us_broadcast(AUDIO_SAMPLE_RATE as f64),
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
            let audio_l = mono.clone();
            let audio_r = mono;
            return StereoFrame {
                audio_left: audio_l,
                audio_right: audio_r,
                scope_left: Vec::new(),
                scope_right: Vec::new(),
                peak_l,
                peak_r: peak_l,
            };
        }

        let mpx = self.fm.process_mpx(iq);
        let (scope_l, scope_r) = self.stereo.process_mpx(&mpx);
        let (mut audio_l, mut audio_r) = self.decimator.process(&scope_l, &scope_r);
        for s in audio_l.iter_mut() {
            *s = self.deemph_l.process(*s as f64) as f32;
        }
        for s in audio_r.iter_mut() {
            *s = self.deemph_r.process(*s as f64) as f32;
        }
        limit_stereo(&mut audio_l, &mut audio_r);
        let peak_l = peak_dbfs(&audio_l);
        let peak_r = peak_dbfs(&audio_r);
        StereoFrame {
            audio_left: audio_l,
            audio_right: audio_r,
            scope_left: scope_l,
            scope_right: scope_r,
            peak_l,
            peak_r,
        }
    }
}

pub struct StereoFrame {
    pub audio_left: Vec<f32>,
    pub audio_right: Vec<f32>,
    pub scope_left: Vec<f32>,
    pub scope_right: Vec<f32>,
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
        let sample_rate = 170_000.0;
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
        let mut de = Deemphasis::us_broadcast(170_000.0);
        let first = de.process(1.0);
        let second = de.process(1.0);
        assert!(second > first);
        assert!(second < 1.0);
    }
}
