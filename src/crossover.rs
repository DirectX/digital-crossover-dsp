use crate::config::AudioRuntimeConfig;

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

/// Per-channel band splitter. Kept as a trait so real FIR/IIR filters
/// can replace the trivial passthrough without touching the DSP loop.
pub trait BandSplitter: Send {
    /// Splits a single input sample into (low, mid, high) band components.
    fn split(&mut self, sample: f32) -> (f32, f32, f32);
}

/// Zero-cost placeholder that routes the full input signal into every band.
/// Band isolation is achieved later through proper filter coefficients;
/// for now per-band gain is the only knob.
pub struct PassthroughSplitter;

impl BandSplitter for PassthroughSplitter {
    #[inline]
    fn split(&mut self, sample: f32) -> (f32, f32, f32) {
        (sample, sample, sample)
    }
}

/// 3-band stereo crossover. Owns per-channel splitters plus live gains.
pub struct Crossover {
    left: Box<dyn BandSplitter>,
    right: Box<dyn BandSplitter>,
    master: f32,
    low_gain: f32,
    mid_gain: f32,
    high_gain: f32,
}

impl Crossover {
    pub fn new(cfg: &AudioRuntimeConfig) -> Self {
        Self {
            left: Box::new(PassthroughSplitter),
            right: Box::new(PassthroughSplitter),
            master: cfg.volume,
            low_gain: cfg.low_gain,
            mid_gain: cfg.mid_gain,
            high_gain: cfg.high_gain,
        }
    }

    /// Hot-update gains from a freshly received config. Filter topology
    /// (cutoffs) will be applied here once real filters are wired in.
    pub fn update(&mut self, cfg: &AudioRuntimeConfig) {
        self.master = cfg.volume;
        self.low_gain = cfg.low_gain;
        self.mid_gain = cfg.mid_gain;
        self.high_gain = cfg.high_gain;
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