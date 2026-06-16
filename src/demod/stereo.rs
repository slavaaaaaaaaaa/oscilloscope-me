//! FM stereo MPX decoder: 19 kHz pilot PLL, L+R / L-R separation.

use super::filters::{Biquad, Deemphasis};
use super::fm::MPX_SAMPLE_RATE;

const PILOT_HZ: f64 = 19_000.0;
const CARRIER_HZ: f64 = 38_000.0;
const PILOT_LOCK_THRESHOLD: f64 = 1e-5;
const PILOT_LOCK_SAMPLES: u32 = 2_000;

pub struct StereoDecoder {
    sample_rate: f64,
    mono_lpf: Biquad,
    pilot_bpf: Biquad,
    carrier_bpf: Biquad,
    stereo_lpf: Biquad,
    deemph_l: Deemphasis,
    deemph_r: Deemphasis,
    pll_phase: f64,
    pll_freq: f64,
    pll_alpha: f64,
    pilot_energy: f64,
    pilot_lock_count: u32,
    stereo: bool,
}

impl StereoDecoder {
    pub fn new() -> Self {
        let sample_rate = MPX_SAMPLE_RATE as f64;
        Self {
            sample_rate,
            mono_lpf: Biquad::lowpass(sample_rate, 15_000.0, 0.707),
            pilot_bpf: Biquad::bandpass(sample_rate, PILOT_HZ, 20.0),
            carrier_bpf: Biquad::bandpass(sample_rate, CARRIER_HZ, 10.0),
            stereo_lpf: Biquad::lowpass(sample_rate, 15_000.0, 0.707),
            deemph_l: Deemphasis::us_broadcast(sample_rate),
            deemph_r: Deemphasis::us_broadcast(sample_rate),
            pll_phase: 0.0,
            pll_freq: 2.0 * std::f64::consts::PI * PILOT_HZ / sample_rate,
            pll_alpha: 0.05,
            pilot_energy: 0.0,
            pilot_lock_count: 0,
            stereo: false,
        }
    }

    pub fn is_stereo(&self) -> bool {
        self.stereo
    }

    pub fn process_mpx(&mut self, mpx: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut left = Vec::with_capacity(mpx.len());
        let mut right = Vec::with_capacity(mpx.len());

        for &sample in mpx {
            let x = sample as f64;
            let mono = self.mono_lpf.process(x);
            let pilot = self.pilot_bpf.process(x);

            self.pilot_energy = 0.999 * self.pilot_energy + 0.001 * pilot * pilot;
            if self.pilot_energy > PILOT_LOCK_THRESHOLD {
                self.pilot_lock_count = self.pilot_lock_count.saturating_add(1);
            } else {
                self.pilot_lock_count = 0;
            }
            self.stereo = self.pilot_lock_count >= PILOT_LOCK_SAMPLES;

            // Track pilot continuously; only decode L-R once energy is present.
            self.track_pilot(pilot);
            let l_minus_r = if self.pilot_energy > PILOT_LOCK_THRESHOLD {
                let doubled = 2.0 * self.pll_phase.sin() * self.pll_phase.cos();
                let carrier = self.carrier_bpf.process(doubled);
                self.stereo_lpf.process(x * carrier)
            } else {
                0.0
            };

            let l = self.deemph_l.process((mono + l_minus_r) * 0.5);
            let r = self.deemph_r.process((mono - l_minus_r) * 0.5);
            left.push(l as f32);
            right.push(r as f32);
        }

        (left, right)
    }

    fn track_pilot(&mut self, pilot: f64) {
        let ref_sin = self.pll_phase.sin();
        let error = pilot * ref_sin;
        self.pll_freq += self.pll_alpha * error;
        self.pll_freq = self.pll_freq.clamp(
            2.0 * std::f64::consts::PI * 18_500.0 / self.sample_rate,
            2.0 * std::f64::consts::PI * 19_500.0 / self.sample_rate,
        );
        self.pll_phase += self.pll_freq;
        if self.pll_phase > std::f64::consts::PI {
            self.pll_phase -= 2.0 * std::f64::consts::PI;
        } else if self.pll_phase < -std::f64::consts::PI {
            self.pll_phase += 2.0 * std::f64::consts::PI;
        }
    }
}
