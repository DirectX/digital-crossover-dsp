use crate::config::AudioRuntimeConfig;

/// Output channel layout (ALSA surround51 convention):
///   0 = FL  -> low  L
///   1 = FR  -> low  R
///   2 = RL  -> mid  L
///   3 = RR  -> mid  R
///   4 = FC  -> high L
///   5 = LFE -> high R
pub const OUT_FL: usize = 0;
pub const OUT_FR: usize = 1;
pub const OUT_RL: usize = 2;
pub const OUT_RR: usize = 3;
pub const OUT_FC: usize = 4;
pub const OUT_LFE: usize = 5;

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
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> [f32; 6] {
        let (l_lo, l_mi, l_hi) = self.left.split(l);
        let (r_lo, r_mi, r_hi) = self.right.split(r);

        let m = self.master;
        let gl = self.low_gain * m;
        let gm = self.mid_gain * m;
        let gh = self.high_gain * m;

        let mut out = [0.0f32; 6];
        out[OUT_FL] = l_lo * gl;
        out[OUT_FR] = r_lo * gl;
        out[OUT_RL] = l_mi * gm;
        out[OUT_RR] = r_mi * gm;
        out[OUT_FC] = l_hi * gh;
        out[OUT_LFE] = r_hi * gh;
        out
    }
}
