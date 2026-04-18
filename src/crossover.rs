use crate::config::{AudioRuntimeConfig, OUTPUT_RATE};

/// Output channel layout — ALSA surround51 stereo-compatible pairs.
/// Each band occupies an L/R pair so stereo imaging is preserved per band.
///
///   0 = FL  (Front Left)   -> Mid  L
///   1 = FR  (Front Right)  -> Mid  R
///   2 = RL  (Rear  Left)   -> High L
///   3 = RR  (Rear  Right)  -> High R
///   4 = FC  (Center)       -> Low  L
///   5 = LFE (Subwoofer)    -> Low  R
pub const OUT_MID_L:  usize = 0;  // FL
pub const OUT_MID_R:  usize = 1;  // FR
pub const OUT_HIGH_L: usize = 2;  // RL
pub const OUT_HIGH_R: usize = 3;  // RR
pub const OUT_LOW_L:  usize = 4;  // FC
pub const OUT_LOW_R:  usize = 5;  // LFE

/// FIR filter length. Odd so the center tap provides an exact integer-sample
/// group delay of (NUM_TAPS-1)/2, which is required for the complementary
/// subtraction trick that keeps the three bands phase-aligned and summing
/// back to the input. 513 taps @ 96 kHz = ~2.7 ms latency and a transition
/// band of roughly 400 Hz around the low cut — plenty for a 3-way crossover.
const NUM_TAPS: usize = 513;

/// Per-channel band splitter. Kept as a trait so real FIR/IIR filters
/// can replace the trivial passthrough without touching the DSP loop.
pub trait BandSplitter: Send {
    /// Splits a single input sample into (low, mid, high) band components.
    fn split(&mut self, sample: f32) -> (f32, f32, f32);
}

/// Zero-cost placeholder that routes the full input signal into every band.
/// Retained as a fallback; real splitting is done by [`FirBandSplitter`].
#[allow(dead_code)]
pub struct PassthroughSplitter;

impl BandSplitter for PassthroughSplitter {
    #[inline]
    fn split(&mut self, sample: f32) -> (f32, f32, f32) {
        (sample, sample, sample)
    }
}

/// Linear-phase FIR 3-band splitter built from two windowed-sinc lowpasses.
///
/// Using two LPFs of identical length (hence identical group delay) lets us
/// derive the mid band by subtraction and recover the high band from the
/// delayed input. The three outputs sum back to `x[n - (NUM_TAPS-1)/2]`.
pub struct FirBandSplitter {
    h_low: Vec<f32>,        // lowpass @ low_cut_hz
    h_mid: Vec<f32>,        // lowpass @ mid_cut_hz
    delay: Vec<f32>,        // circular buffer of past input samples
    pos: usize,             // write index (most recent sample lives here)
    center: usize,          // (NUM_TAPS - 1) / 2
    low_cut_hz: f32,
    mid_cut_hz: f32,
    sample_rate: f32,
}

impl FirBandSplitter {
    pub fn new(low_cut_hz: f32, mid_cut_hz: f32, sample_rate: f32) -> Self {
        let (lo, mi) = sanitize_cuts(low_cut_hz, mid_cut_hz, sample_rate);
        Self {
            h_low: design_lowpass(lo, sample_rate, NUM_TAPS),
            h_mid: design_lowpass(mi, sample_rate, NUM_TAPS),
            delay: vec![0.0; NUM_TAPS],
            pos: 0,
            center: (NUM_TAPS - 1) / 2,
            low_cut_hz: lo,
            mid_cut_hz: mi,
            sample_rate,
        }
    }

    /// Rebuild coefficients in place when cutoffs change. Preserves the
    /// delay-line history so there's no audible click on config updates.
    pub fn set_cutoffs(&mut self, low_cut_hz: f32, mid_cut_hz: f32) {
        let (lo, mi) = sanitize_cuts(low_cut_hz, mid_cut_hz, self.sample_rate);
        if (lo - self.low_cut_hz).abs() > f32::EPSILON {
            self.h_low = design_lowpass(lo, self.sample_rate, NUM_TAPS);
            self.low_cut_hz = lo;
        }
        if (mi - self.mid_cut_hz).abs() > f32::EPSILON {
            self.h_mid = design_lowpass(mi, self.sample_rate, NUM_TAPS);
            self.mid_cut_hz = mi;
        }
    }
}

impl BandSplitter for FirBandSplitter {
    #[inline]
    fn split(&mut self, sample: f32) -> (f32, f32, f32) {
        let n = self.delay.len();
        self.delay[self.pos] = sample;

        // Convolve: h[k] multiplies x[n-k]. Most recent sample (k=0) is at
        // delay[pos], so we walk the ring backwards from pos.
        let mut y_low = 0.0f32;
        let mut y_mid = 0.0f32;
        let mut idx = self.pos;
        for k in 0..n {
            let x = self.delay[idx];
            y_low += self.h_low[k] * x;
            y_mid += self.h_mid[k] * x;
            idx = if idx == 0 { n - 1 } else { idx - 1 };
        }

        // Sample aligned with the filters' group delay — needed so the high
        // band subtraction cancels the passband of the mid lowpass exactly.
        let center_idx = (self.pos + n - self.center) % n;
        let x_delayed = self.delay[center_idx];

        self.pos = if self.pos + 1 == n { 0 } else { self.pos + 1 };

        let low = y_low;
        let mid = y_mid - y_low;
        let high = x_delayed - y_mid;
        (low, mid, high)
    }
}

/// Windowed-sinc lowpass design. Blackman window gives ~-74 dB stopband,
/// which is well below anything audible after per-band gain staging.
fn design_lowpass(cutoff_hz: f32, sample_rate: f32, num_taps: usize) -> Vec<f32> {
    use std::f32::consts::PI;
    let m = (num_taps - 1) as f32;
    let fc = (cutoff_hz / sample_rate).clamp(1.0e-6, 0.499);
    let mut h = vec![0.0f32; num_taps];
    let mut sum = 0.0f32;
    for n in 0..num_taps {
        let x = n as f32 - m / 2.0;
        // Ideal lowpass impulse response: 2*fc * sinc(2*fc*x)
        let ideal = if x.abs() < 1.0e-9 {
            2.0 * fc
        } else {
            (2.0 * PI * fc * x).sin() / (PI * x)
        };
        // Blackman window
        let w = 0.42 - 0.5 * (2.0 * PI * n as f32 / m).cos()
                     + 0.08 * (4.0 * PI * n as f32 / m).cos();
        h[n] = ideal * w;
        sum += h[n];
    }
    // Normalize DC gain to exactly 1.0 so band sums stay unity.
    let inv = 1.0 / sum;
    for v in h.iter_mut() {
        *v *= inv;
    }
    h
}

/// Guard against nonsensical cutoff configs (crossed, zero, above Nyquist).
fn sanitize_cuts(low: f32, mid: f32, sample_rate: f32) -> (f32, f32) {
    let nyq = sample_rate * 0.5;
    let mut lo = low.clamp(10.0, nyq - 1.0);
    let mut mi = mid.clamp(10.0, nyq - 1.0);
    if mi <= lo {
        // Keep a minimum octave gap so the mid band isn't empty.
        mi = (lo * 2.0).min(nyq - 1.0);
        if mi <= lo {
            lo = mi * 0.5;
        }
    }
    (lo, mi)
}

/// 3-band stereo crossover. Owns per-channel splitters plus live gains.
pub struct Crossover {
    left: FirBandSplitter,
    right: FirBandSplitter,
    master: f32,
    low_gain: f32,
    mid_gain: f32,
    high_gain: f32,
}

impl Crossover {
    pub fn new(cfg: &AudioRuntimeConfig) -> Self {
        let sr = OUTPUT_RATE as f32;
        Self {
            left: FirBandSplitter::new(cfg.low_cut_hz, cfg.mid_cut_hz, sr),
            right: FirBandSplitter::new(cfg.low_cut_hz, cfg.mid_cut_hz, sr),
            master: cfg.volume,
            low_gain: cfg.low_gain,
            mid_gain: cfg.mid_gain,
            high_gain: cfg.high_gain,
        }
    }

    /// Hot-update gains and crossover frequencies from a freshly received
    /// config. Filter coefficients are recomputed in place only when the
    /// cutoffs actually change.
    pub fn update(&mut self, cfg: &AudioRuntimeConfig) {
        self.master = cfg.volume;
        self.low_gain = cfg.low_gain;
        self.mid_gain = cfg.mid_gain;
        self.high_gain = cfg.high_gain;
        self.left.set_cutoffs(cfg.low_cut_hz, cfg.mid_cut_hz);
        self.right.set_cutoffs(cfg.low_cut_hz, cfg.mid_cut_hz);
    }

    /// Process a stereo frame and emit a 6-channel frame in surround51 order.
    /// Band assignment: Mid→FL/FR, High→RL/RR, Low→FC/LFE.
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> [f32; 6] {
        let (l_lo, l_mi, l_hi) = self.left.split(l);
        let (r_lo, r_mi, r_hi) = self.right.split(r);

        let m = self.master;
        let gl = self.low_gain * m;
        let gm = self.mid_gain * m;
        let gh = self.high_gain * m;

        let mut out = [0.0f32; 6];
        out[OUT_MID_L]  = l_mi * gm;
        out[OUT_MID_R]  = r_mi * gm;
        out[OUT_HIGH_L] = l_hi * gh;
        out[OUT_HIGH_R] = r_hi * gh;
        out[OUT_LOW_L]  = l_lo * gl;
        out[OUT_LOW_R]  = r_lo * gl;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Impulse response should sum to the delayed input across all three
    /// bands (this is the whole point of the complementary FIR structure).
    #[test]
    fn bands_sum_to_delayed_input() {
        let mut s = FirBandSplitter::new(1000.0, 10000.0, 96000.0);
        let center = (NUM_TAPS - 1) / 2;
        // Push an impulse followed by zeros; reconstructed signal should be
        // a single unit sample at index `center`.
        let mut recon = vec![0.0f32; NUM_TAPS + 16];
        for n in 0..recon.len() {
            let x = if n == 0 { 1.0 } else { 0.0 };
            let (lo, mi, hi) = s.split(x);
            recon[n] = lo + mi + hi;
        }
        for (i, v) in recon.iter().enumerate() {
            let expect = if i == center { 1.0 } else { 0.0 };
            assert!((v - expect).abs() < 1.0e-5, "recon[{i}]={v} expected {expect}");
        }
    }

    /// DC (a constant) must pass entirely through the low band.
    #[test]
    fn dc_lives_in_low_band() {
        let mut s = FirBandSplitter::new(1000.0, 10000.0, 96000.0);
        // Prime the delay line so the FIRs reach steady state.
        for _ in 0..NUM_TAPS * 2 { s.split(1.0); }
        let (lo, mi, hi) = s.split(1.0);
        assert!((lo - 1.0).abs() < 1.0e-4, "low={lo}");
        assert!(mi.abs() < 1.0e-4, "mid={mi}");
        assert!(hi.abs() < 1.0e-4, "high={hi}");
    }
}
