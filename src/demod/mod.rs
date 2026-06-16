mod filters;
mod fm;
mod stereo;

pub use fm::{configure_sdr, optimal_settings, FmDemodulator, MPX_SAMPLE_RATE, RadioConfig};
pub use stereo::StereoDecoder;

pub struct DemodPipeline {
    fm: FmDemodulator,
    stereo: StereoDecoder,
}

impl DemodPipeline {
    pub fn new(downsample: u32) -> Self {
        Self {
            fm: FmDemodulator::new(downsample),
            stereo: StereoDecoder::new(),
        }
    }

    pub fn process_iq(&mut self, iq: &[u8]) -> StereoFrame {
        let mpx = self.fm.process_iq(iq);
        let (left, right) = self.stereo.process_mpx(&mpx);
        StereoFrame {
            left,
            right,
            is_stereo: self.stereo.is_stereo(),
        }
    }
}

pub struct StereoFrame {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub is_stereo: bool,
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

#[cfg(test)]
mod tests {
    use super::filters::{Biquad, Deemphasis};

    #[test]
    fn lowpass_attenuates_high_frequency() {
        let sample_rate = 228_000.0;
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
        let mut de = Deemphasis::us_broadcast(228_000.0);
        let first = de.process(1.0);
        let second = de.process(1.0);
        assert!(second > first);
        assert!(second < 1.0);
    }
}
