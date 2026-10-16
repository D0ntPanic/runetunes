//! Windowed-sinc filter tables for [`Interpolation::Sinc`](super::voice::Interpolation::Sinc).
//!
//! A voice resamples its sample by evaluating a band-limited reconstruction of it at each
//! output position: a Kaiser-windowed sinc kernel, tabulated at [`PHASES`] fractional offsets
//! and linearly blended between them.
//!
//! When a voice plays faster than one sample per output sample, the sample's content above the
//! output Nyquist frequency would alias, so the cutoff is lowered in proportion to the step
//! (and the kernel widened to keep its quality). There is one table per semitone of step above
//! 1, up to [`MAX_RATIO`]; beyond that the cutoff stays at its lowest.

use std::sync::OnceLock;

/// Taps of the kernel used when the step is at most one sample.
const BASE_HALF_TAPS: usize = 12;

/// Cutoff relative to the sample's Nyquist frequency, leaving room for the transition band.
const BASE_CUTOFF: f64 = 0.95;

/// Kaiser window shape parameter. With 24 taps this keeps images below -90 dB while staying
/// within 0.2 dB up to 80% of the sample's Nyquist frequency.
const KAISER_BETA: f64 = 9.0;

/// Number of fractional offsets the kernel is tabulated at, as a power of two.
pub const PHASE_BITS: u32 = 8;
const PHASES: usize = 1 << PHASE_BITS;

/// Bands per doubling of the step.
const BANDS_PER_OCTAVE: f64 = 12.0;

/// Largest step (in samples per output sample) with a correctly band-limited table.
const MAX_RATIO: f64 = 4.0;

/// Filter coefficients for one cutoff.
pub struct SincTable {
    /// Number of taps; tap `i` applies to sample `floor(position) - half + 1 + i`.
    pub taps: usize,
    /// Half the number of taps.
    pub half: usize,
    /// `PHASES + 1` rows of `taps` coefficients, for fractional offsets `0..=1`.
    coefficients: Vec<f32>,
}

impl SincTable {
    fn new(half: usize, cutoff: f64) -> Self {
        let taps = half * 2;
        let mut coefficients = Vec::with_capacity((PHASES + 1) * taps);
        for phase in 0..=PHASES {
            let fraction = phase as f64 / PHASES as f64;
            let row: Vec<f64> = (0..taps)
                .map(|i| {
                    let t = i as f64 - (half as f64 - 1.0) - fraction;
                    let x = std::f64::consts::PI * cutoff * t;
                    let sinc = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
                    let w = t / half as f64;
                    let window = if w.abs() >= 1.0 {
                        0.0
                    } else {
                        bessel_i0(KAISER_BETA * (1.0 - w * w).sqrt()) / bessel_i0(KAISER_BETA)
                    };
                    sinc * window
                })
                .collect();
            // Normalize every phase to unity gain at DC, so the gain doesn't ripple with the
            // fractional position.
            let sum: f64 = row.iter().sum();
            coefficients.extend(row.iter().map(|&c| (c / sum) as f32));
        }
        SincTable {
            taps,
            half,
            coefficients,
        }
    }

    /// The coefficient rows for a phase and the next one.
    #[inline(always)]
    pub fn rows(&self, phase: usize) -> (&[f32], &[f32]) {
        let start = phase * self.taps;
        (
            &self.coefficients[start..start + self.taps],
            &self.coefficients[start + self.taps..start + self.taps * 2],
        )
    }

    /// The table to use when playing `ratio` samples per output sample.
    pub fn for_ratio(ratio: f64) -> &'static SincTable {
        let tables = TABLES.get_or_init(|| {
            (0..=band_of(MAX_RATIO))
                .map(|band| {
                    let scale = 2f64.powf(band as f64 / BANDS_PER_OCTAVE);
                    SincTable::new(
                        (BASE_HALF_TAPS as f64 * scale).ceil() as usize,
                        BASE_CUTOFF / scale,
                    )
                })
                .collect()
        });
        &tables[band_of(ratio.min(MAX_RATIO))]
    }
}

static TABLES: OnceLock<Vec<SincTable>> = OnceLock::new();

/// The band whose cutoff is low enough for `ratio`.
fn band_of(ratio: f64) -> usize {
    if ratio <= 1.0 {
        0
    } else {
        (BANDS_PER_OCTAVE * ratio.log2() - 1e-9).ceil() as usize
    }
}

/// Modified Bessel function of the first kind, order zero.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let mut k = 1.0;
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        k += 1.0;
    }
    sum
}
