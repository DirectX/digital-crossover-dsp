use crate::config::AudioRuntimeConfig;

/// Output channel layout.
///
/// Almost every 5.1 sink (HDMI receivers, USB DACs exposing 6ch, PipeWire,
/// PulseAudio, WAVEFORMATEXTENSIBLE) uses this canonical interleaved order:
///
///   0 = FL   (Front Left)    -> Mid  L
///   1 = FR   (Front Right)   -> Mid  R
///   2 = FC   (Center)        -> Low  L
///   3 = LFE  (Subwoofer)     -> Low  R
///   4 = RL   (Rear  Left)    -> High L
///   5 = RR   (Rear  Right)   -> High R
///
/// ALSA's legacy `surround51` device uses a different order (FL,FR,RL,RR,
/// FC,LFE); do NOT open that device. Prefer `default`, `pipewire`, or a raw
/// `hw:` node whose native order is the one above.
pub const OUT_MID_L:  usize = 0;  // FL
pub const OUT_MID_R:  usize = 1;  // FR
pub const OUT_LOW_L:  usize = 2;  // FC
pub const OUT_LOW_R:  usize = 3;  // LFE
pub const OUT_HIGH_L: usize = 4;  // RL
pub const OUT_HIGH_R: usize = 5;  // RR

/// Per-channel band splitter. Kept as a trait so alternate filter
/// topologies can be swapped in without touching the DSP loop.
pub trait BandSplitter: Send {
    fn split(&mut self, sample: f32) -> (f32, f32, f32);
}

// ---------------------------------------------------------------------------
// Biquad (Direct Form I, RBJ cookbook conventions)
// ---------------------------------------------------------------------------
//
// A single 2nd-order IIR section. We cascade two Butterworth biquads per
// band edge to build a Linkwitz-Riley 4th-order response (24 dB/oct).
// LR4 is the de-facto standard for loudspeaker crossovers: minimum-phase
// (no pre-ringing), -6 dB at the crossover point on both sides, and
// adjacent bands sum to a flat all-pass magnitude response — exactly what
// eliminates the comb-filter/hollow artifacts a linear-phase FIR produces
// when its bands feed physically separated drivers.

#[derive(Clone, Copy)]
struct Biquad {
    b0: f32, b1: f32, b2: f32,
    a1: f32, a2: f32,
    x1: f32, x2: f32,
    y1: f32, y2: f32,
}

impl Biquad {
    #[allow(dead_code)]
    fn identity() -> Self {
        Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0,
               x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    /// 2nd-order Butterworth lowpass (Q = 1/√2). Two of these in series
    /// give a Linkwitz-Riley 4th-order lowpass.
    fn butterworth_lpf(fc: f32, fs: f32) -> Self {
        use std::f32::consts::{PI, FRAC_1_SQRT_2};
        let w0 = 2.0 * PI * fc / fs;
        let (s, c) = w0.sin_cos();
        // Q = 1/√2  ⇒  α = sin(w0) / (2Q) = sin(w0) / √2
        let alpha = s * FRAC_1_SQRT_2;
        let a0 =  1.0 + alpha;
        let b0 = ((1.0 - c) * 0.5) / a0;
        let b1 =  (1.0 - c) / a0;
        let b2 = ((1.0 - c) * 0.5) / a0;
        let a1 = (-2.0 * c) / a0;
        let a2 =  (1.0 - alpha) / a0;
        Self { b0, b1, b2, a1, a2, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    /// 2nd-order Butterworth highpass (Q = 1/√2).
    fn butterworth_hpf(fc: f32, fs: f32) -> Self {
        use std::f32::consts::{PI, FRAC_1_SQRT_2};
        let w0 = 2.0 * PI * fc / fs;
        let (s, c) = w0.sin_cos();
        let alpha = s * FRAC_1_SQRT_2;
        let a0 =  1.0 + alpha;
        let b0 =  ((1.0 + c) * 0.5) / a0;
        let b1 = -(1.0 + c) / a0;
        let b2 =  ((1.0 + c) * 0.5) / a0;
        let a1 =  (-2.0 * c) / a0;
        let a2 =   (1.0 - alpha) / a0;
        Self { b0, b1, b2, a1, a2, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
                            - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Linkwitz-Riley 4th-order section = two cascaded Butterworth biquads.
#[derive(Clone, Copy)]
struct Lr4 { a: Biquad, b: Biquad }

impl Lr4 {
    fn lpf(fc: f32, fs: f32) -> Self {
        Self { a: Biquad::butterworth_lpf(fc, fs), b: Biquad::butterworth_lpf(fc, fs) }
    }
    fn hpf(fc: f32, fs: f32) -> Self {
        Self { a: Biquad::butterworth_hpf(fc, fs), b: Biquad::butterworth_hpf(fc, fs) }
    }
    #[allow(dead_code)]
    fn identity() -> Self { Self { a: Biquad::identity(), b: Biquad::identity() } }
    #[inline(always)]
    fn process(&mut self, x: f32) -> f32 { self.b.process(self.a.process(x)) }
}

// ---------------------------------------------------------------------------
// 3-way Linkwitz-Riley band splitter
// ---------------------------------------------------------------------------
//
// Topology:
//     low  = LPF_low (x)
//     mid  = LPF_mid ( HPF_low (x) )      // band-pass via serial chain
//     high = HPF_mid (x)
//
// Each LPF/HPF is LR4 (24 dB/oct). Summed magnitude of (low + mid + high)
// is flat within a fraction of a dB; phase rotates smoothly through the
// crossover region (no pre-echo, no comb filter). This is the classic
// textbook 3-way LR layout used in every serious active loudspeaker.

pub struct LrBandSplitter {
    lpf_low:  Lr4,        // extracts the low band
    hpf_low:  Lr4,        // feeds into the mid chain
    lpf_mid:  Lr4,        // caps the mid band at the upper cut
    hpf_mid:  Lr4,        // extracts the high band
    low_cut_hz: f32,
    mid_cut_hz: f32,
    sample_rate: f32,
}

impl LrBandSplitter {
    pub fn new(low_cut_hz: f32, mid_cut_hz: f32, sample_rate: f32) -> Self {
        let (lo, mi) = sanitize_cuts(low_cut_hz, mid_cut_hz, sample_rate);
        Self {
            lpf_low: Lr4::lpf(lo, sample_rate),
            hpf_low: Lr4::hpf(lo, sample_rate),
            lpf_mid: Lr4::lpf(mi, sample_rate),
            hpf_mid: Lr4::hpf(mi, sample_rate),
            low_cut_hz: lo,
            mid_cut_hz: mi,
            sample_rate,
        }
    }

    /// Rebuild coefficients on cutoff changes. We replace only the affected
    /// sections, keeping the filter state (x/y history) so there's no click.
    pub fn set_cutoffs(&mut self, low_cut_hz: f32, mid_cut_hz: f32) {
        let (lo, mi) = sanitize_cuts(low_cut_hz, mid_cut_hz, self.sample_rate);
        if (lo - self.low_cut_hz).abs() > f32::EPSILON {
            update_lr_coeffs(&mut self.lpf_low, Biquad::butterworth_lpf(lo, self.sample_rate));
            update_lr_coeffs(&mut self.hpf_low, Biquad::butterworth_hpf(lo, self.sample_rate));
            self.low_cut_hz = lo;
        }
        if (mi - self.mid_cut_hz).abs() > f32::EPSILON {
            update_lr_coeffs(&mut self.lpf_mid, Biquad::butterworth_lpf(mi, self.sample_rate));
            update_lr_coeffs(&mut self.hpf_mid, Biquad::butterworth_hpf(mi, self.sample_rate));
            self.mid_cut_hz = mi;
        }
    }
}

impl BandSplitter for LrBandSplitter {
    #[inline]
    fn split(&mut self, sample: f32) -> (f32, f32, f32) {
        let low  = self.lpf_low.process(sample);
        let mid  = self.lpf_mid.process(self.hpf_low.process(sample));
        let high = self.hpf_mid.process(sample);
        (low, mid, high)
    }
}

/// Swap in new biquad coefficients without clearing the delay-line state,
/// so a live cutoff change doesn't produce a discontinuity.
fn update_lr_coeffs(lr: &mut Lr4, src: Biquad) {
    for target in [&mut lr.a, &mut lr.b] {
        target.b0 = src.b0; target.b1 = src.b1; target.b2 = src.b2;
        target.a1 = src.a1; target.a2 = src.a2;
    }
}

/// Guard against nonsensical cutoff configs (crossed, zero, above Nyquist).
fn sanitize_cuts(low: f32, mid: f32, sample_rate: f32) -> (f32, f32) {
    let nyq = sample_rate * 0.5;
    let mut lo = low.clamp(10.0, nyq - 1.0);
    let mut mi = mid.clamp(10.0, nyq - 1.0);
    if mi <= lo {
        mi = (lo * 2.0).min(nyq - 1.0);
        if mi <= lo { lo = mi * 0.5; }
    }
    (lo, mi)
}

/// 3-band stereo crossover. Owns per-channel splitters plus live gains.
pub struct Crossover {
    left:  LrBandSplitter,
    right: LrBandSplitter,
    master: f32,
    low_gain: f32,
    mid_gain: f32,
    high_gain: f32,
    low_mute: bool,
    mid_mute: bool,
    high_mute: bool,
    low_solo: bool,
    mid_solo: bool,
    high_solo: bool,
    low_bypass: bool,
    mid_bypass: bool,
    high_bypass: bool,
}

impl Crossover {
    pub fn new(cfg: &AudioRuntimeConfig, sample_rate: f32) -> Self {
        let sr = sample_rate;
        Self {
            left:  LrBandSplitter::new(cfg.low_cut_hz, cfg.mid_cut_hz, sr),
            right: LrBandSplitter::new(cfg.low_cut_hz, cfg.mid_cut_hz, sr),
            master: cfg.volume,
            low_gain: cfg.low_gain,
            mid_gain: cfg.mid_gain,
            high_gain: cfg.high_gain,
            low_mute: cfg.low_mute,
            mid_mute: cfg.mid_mute,
            high_mute: cfg.high_mute,
            low_solo: cfg.low_solo,
            mid_solo: cfg.mid_solo,
            high_solo: cfg.high_solo,
            low_bypass: cfg.low_bypass,
            mid_bypass: cfg.mid_bypass,
            high_bypass: cfg.high_bypass,
        }
    }

    pub fn update(&mut self, cfg: &AudioRuntimeConfig) {
        self.master = cfg.volume;
        self.low_gain = cfg.low_gain;
        self.mid_gain = cfg.mid_gain;
        self.high_gain = cfg.high_gain;
        self.low_mute = cfg.low_mute;
        self.mid_mute = cfg.mid_mute;
        self.high_mute = cfg.high_mute;
        self.low_solo = cfg.low_solo;
        self.mid_solo = cfg.mid_solo;
        self.high_solo = cfg.high_solo;
        self.low_bypass = cfg.low_bypass;
        self.mid_bypass = cfg.mid_bypass;
        self.high_bypass = cfg.high_bypass;
        self.left.set_cutoffs(cfg.low_cut_hz, cfg.mid_cut_hz);
        self.right.set_cutoffs(cfg.low_cut_hz, cfg.mid_cut_hz);
    }

    /// Process a stereo frame and emit a 6-channel frame in surround51 order.
    /// Band assignment: Mid→FL/FR, High→RL/RR, Low→FC/LFE.
    ///
    /// Solo: if any band is soloed, only soloed bands produce output.
    /// Mute: silence the band regardless of solo state.
    /// Bypass: substitute the raw (pre-filter) input for the filtered signal.
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> [f32; 6] {
        let (l_lo, l_mi, l_hi) = self.left.split(l);
        let (r_lo, r_mi, r_hi) = self.right.split(r);

        let any_solo = self.low_solo || self.mid_solo || self.high_solo;
        let m = self.master;

        let active_lo = (!any_solo || self.low_solo) && !self.low_mute;
        let active_mi = (!any_solo || self.mid_solo) && !self.mid_mute;
        let active_hi = (!any_solo || self.high_solo) && !self.high_mute;

        let (sl, sr_) = if self.low_bypass  { (l, r) } else { (l_lo, r_lo) };
        let (ml, mr_) = if self.mid_bypass  { (l, r) } else { (l_mi, r_mi) };
        let (hl, hr_) = if self.high_bypass { (l, r) } else { (l_hi, r_hi) };

        let gl = if active_lo { self.low_gain  * m } else { 0.0 };
        let gm = if active_mi { self.mid_gain  * m } else { 0.0 };
        let gh = if active_hi { self.high_gain * m } else { 0.0 };

        let mut out = [0.0f32; 6];
        out[OUT_MID_L]  = ml  * gm;
        out[OUT_MID_R]  = mr_ * gm;
        out[OUT_HIGH_L] = hl  * gh;
        out[OUT_HIGH_R] = hr_ * gh;
        out[OUT_LOW_L]  = sl  * gl;
        out[OUT_LOW_R]  = sr_ * gl;
        out
    }
}

// Keep the trivial splitter around as a fallback / sanity target for bring-up.
#[allow(dead_code)]
pub struct PassthroughSplitter;
impl BandSplitter for PassthroughSplitter {
    #[inline]
    fn split(&mut self, s: f32) -> (f32, f32, f32) { (s, s, s) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn prime<S: BandSplitter>(s: &mut S, x: f32, n: usize) {
        for _ in 0..n { s.split(x); }
    }

    /// DC (a constant) must end up entirely in the low band.
    #[test]
    fn dc_lives_in_low_band() {
        let mut s = LrBandSplitter::new(500.0, 5000.0, 96000.0);
        prime(&mut s, 1.0, 4096);
        let (lo, mi, hi) = s.split(1.0);
        assert!((lo - 1.0).abs() < 1.0e-3, "low={lo}");
        assert!(mi.abs() < 1.0e-3, "mid={mi}");
        assert!(hi.abs() < 1.0e-3, "high={hi}");
    }

    /// Nyquist-ish alternating signal must end up entirely in the high band.
    #[test]
    fn hf_lives_in_high_band() {
        let mut s = LrBandSplitter::new(500.0, 5000.0, 96000.0);
        // Feed a 24 kHz square-ish signal (alternating sign) for a while.
        let mut sign = 1.0f32;
        for _ in 0..4096 { s.split(sign); sign = -sign; }
        // Now measure: bands' energy over another block.
        let (mut el, mut em, mut eh) = (0.0f32, 0.0f32, 0.0f32);
        for _ in 0..4096 {
            let (lo, mi, hi) = s.split(sign);
            sign = -sign;
            el += lo * lo; em += mi * mi; eh += hi * hi;
        }
        assert!(eh > 100.0 * (el + em), "energies: low={el} mid={em} high={eh}");
    }

    /// 3-way LR4 is all-pass in magnitude: summed bands should preserve the
    /// RMS level of a broadband input to within a small tolerance.
    #[test]
    fn bands_sum_is_approximately_allpass() {
        let mut s = LrBandSplitter::new(500.0, 5000.0, 96000.0);
        // Deterministic pseudo-noise.
        let mut state: u32 = 0x1234_5678;
        let mut rng = || {
            state ^= state << 13; state ^= state >> 17; state ^= state << 5;
            (state as i32 as f32) / (i32::MAX as f32)
        };
        // Prime past the transient.
        for _ in 0..4096 { let x = rng(); s.split(x); }
        let (mut ex, mut ey) = (0.0f64, 0.0f64);
        for _ in 0..16384 {
            let x = rng();
            let (lo, mi, hi) = s.split(x);
            let y = lo + mi + hi;
            ex += (x * x) as f64;
            ey += (y * y) as f64;
        }
        let ratio = ey / ex;
        assert!((ratio - 1.0).abs() < 0.02, "sum/input energy ratio = {ratio}");
    }
}
