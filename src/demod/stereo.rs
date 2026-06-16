//! FM stereo MPX decoder: 19 kHz pilot PLL, L+R / L-R separation.

use super::filters::Biquad;
use super::fm::MPX_SAMPLE_RATE;

const PILOT_HZ: f64 = 19_000.0;
const PLL_BW_HZ: f64 = 50.0;

pub struct StereoDecoder {
    lpr_lpf: Biquad,
    phasor_lpf_i: Biquad,
    phasor_lpf_q: Biquad,
    lmr_sin_lpf: Biquad,
    lmr_cos_lpf: Biquad,
    pll_phase: f64,
    pll_freq: f64,
    min_freq: f64,
    max_freq: f64,
    loop_b0: f64,
    loop_b1: f64,
    loop_x1: f64,
    pilot_lock_count: u32,
    use_cos_carrier: Option<bool>,
    sin_energy: f64,
    cos_energy: f64,
}

impl StereoDecoder {
    pub fn new() -> Self {
        let sample_rate = MPX_SAMPLE_RATE as f64;
        let pilot_norm = PILOT_HZ / sample_rate;
        let bw_norm = PLL_BW_HZ / sample_rate;
        let bw = bw_norm * 2.0 * std::f64::consts::PI;
        let q1 = (-0.1153 * bw).exp();
        Self {
            lpr_lpf: Biquad::lowpass(sample_rate, 15_000.0, 0.707),
            phasor_lpf_i: Biquad::lowpass(sample_rate, 500.0, 0.707),
            phasor_lpf_q: Biquad::lowpass(sample_rate, 500.0, 0.707),
            lmr_sin_lpf: Biquad::lowpass(sample_rate, 15_000.0, 0.707),
            lmr_cos_lpf: Biquad::lowpass(sample_rate, 15_000.0, 0.707),
            pll_phase: 0.0,
            pll_freq: pilot_norm * 2.0 * std::f64::consts::PI,
            min_freq: (pilot_norm - bw_norm) * 2.0 * std::f64::consts::PI,
            max_freq: (pilot_norm + bw_norm) * 2.0 * std::f64::consts::PI,
            loop_b0: 0.62 * bw,
            loop_b1: -0.62 * bw * q1,
            loop_x1: 0.0,
            pilot_lock_count: 0,
            use_cos_carrier: None,
            sin_energy: 0.0,
            cos_energy: 0.0,
        }
    }

    /// Decode MPX to L/R at MPX rate (no de-emphasis — applied after decimation).
    pub fn process_mpx(&mut self, mpx: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut left = Vec::with_capacity(mpx.len());
        let mut right = Vec::with_capacity(mpx.len());

        for &sample in mpx {
            let x = sample as f64;
            let lpr = self.lpr_lpf.process(x);
            let (sin2, cos2, _phasor_i) = self.track_pilot(x);

            let lmr_sin = self.lmr_sin_lpf.process(2.0 * x * sin2);
            let lmr_cos = self.lmr_cos_lpf.process(2.0 * x * cos2);

            self.pilot_lock_count = self.pilot_lock_count.saturating_add(1);
            self.sin_energy = 0.999 * self.sin_energy + 0.001 * lmr_sin * lmr_sin;
            self.cos_energy = 0.999 * self.cos_energy + 0.001 * lmr_cos * lmr_cos;
            if self.use_cos_carrier.is_none() && self.pilot_lock_count >= 8_000 {
                self.use_cos_carrier = Some(self.cos_energy > self.sin_energy);
            }

            let lmr = match self.use_cos_carrier {
                Some(true) => lmr_cos,
                _ => lmr_sin,
            };

            left.push(((lpr + lmr) * 0.5) as f32);
            right.push(((lpr - lmr) * 0.5) as f32);
        }

        (left, right)
    }

    fn track_pilot(&mut self, mpx: f64) -> (f64, f64, f64) {
        let psin = self.pll_phase.sin();
        let pcos = self.pll_phase.cos();
        let sin2 = 2.0 * psin * pcos;
        let cos2 = pcos * pcos - psin * psin;

        let phasor_i = self.phasor_lpf_i.process(psin * mpx);
        let phasor_q = self.phasor_lpf_q.process(pcos * mpx);

        let phase_err = if phasor_i.abs() > phasor_q.abs() {
            phasor_q / phasor_i
        } else if phasor_q > 0.0 {
            1.0
        } else {
            -1.0
        };

        self.pll_freq += self.loop_b0 * phase_err + self.loop_b1 * self.loop_x1;
        self.loop_x1 = phase_err;
        self.pll_freq = self.pll_freq.clamp(self.min_freq, self.max_freq);

        self.pll_phase += self.pll_freq;
        if self.pll_phase > 2.0 * std::f64::consts::PI {
            self.pll_phase -= 2.0 * std::f64::consts::PI;
        }

        (sin2, cos2, phasor_i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_mpx(
        sample_rate: f64,
        n: usize,
        subcarrier: fn(f64) -> f64,
    ) -> Vec<f32> {
        let mut mpx = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sample_rate;
            let l = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin();
            let r = 0.3 * (2.0 * std::f64::consts::PI * 880.0 * t).sin();
            let lpr = l + r;
            let lmr = l - r;
            let pilot = 0.09 * (2.0 * std::f64::consts::PI * PILOT_HZ * t).sin();
            let sub = lmr * subcarrier(2.0 * std::f64::consts::PI * PILOT_HZ * 2.0 * t);
            mpx.push((lpr + pilot + sub) as f32);
        }
        mpx
    }

    fn assert_separated(left: &[f32], right: &[f32]) {
        let corr_lr: f64 = left
            .iter()
            .zip(right.iter())
            .map(|(&a, &b)| a as f64 * b as f64)
            .sum::<f64>()
            / left.len() as f64;
        let var_l: f64 = left.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / left.len() as f64;
        let var_r: f64 = right.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / right.len() as f64;
        assert!(var_l > 1e-8 && var_r > 1e-8);
        assert!(
            corr_lr.abs() < 0.5 * var_l.min(var_r),
            "L and R should differ; corr={corr_lr} var_l={var_l} var_r={var_r}"
        );
    }

    #[test]
    fn stereo_decoder_sin_subcarrier() {
        let sr = MPX_SAMPLE_RATE as f64;
        let n = 80_000;
        let mpx = synth_mpx(sr, n, |wt| wt.sin());
        let mut decoder = StereoDecoder::new();
        let (left, right) = decoder.process_mpx(&mpx);
        assert_separated(&left[n / 2..], &right[n / 2..]);
    }

    #[test]
    fn stereo_decoder_cos_subcarrier() {
        let sr = MPX_SAMPLE_RATE as f64;
        let n = 80_000;
        let mpx = synth_mpx(sr, n, |wt| wt.cos());
        let mut decoder = StereoDecoder::new();
        let (left, right) = decoder.process_mpx(&mpx);
        assert_separated(&left[n / 2..], &right[n / 2..]);
    }
}
