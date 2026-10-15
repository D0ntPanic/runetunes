//! Time-varying IIR filter used by the procedural synthesizer.

use super::envelope::SynthEnvelope;
use crate::buffer::Buffer;
use anyhow::Result;

/// Index of the feedforward (zero) section in the per-direction arrays.
pub const FEEDFORWARD: usize = 0;
/// Index of the feedback (pole) section in the per-direction arrays.
pub const FEEDBACK: usize = 1;

/// A cascade of up to four complex-conjugate pole pairs and four zero pairs whose positions are
/// interpolated between a "start" and "end" configuration by the tone's filter envelope.
///
/// Each pair is described by a frequency (in 1/8192 octave steps above C1, 32.7 Hz) and a
/// magnitude (how close the pole or zero sits to the unit circle). The filter is always designed
/// for an 11025 Hz sample rate, regardless of the rate the tone is rendered at.
#[derive(Debug, Clone, Default)]
pub struct SynthFilter {
    /// Number of zero pairs (`[FEEDFORWARD]`) and pole pairs (`[FEEDBACK]`).
    pub pair_counts: [usize; 2],
    /// Pair magnitudes, indexed by `[section][start/end][pair]`.
    magnitudes: [[[i32; 4]; 2]; 2],
    /// Pair frequencies in 1/8192 octaves above C1, indexed by `[section][start/end][pair]`.
    frequencies: [[[i32; 4]; 2]; 2],
    /// Output attenuation at the start and end of the envelope, in units of 1/327.68 dB.
    attenuation: [i32; 2],
}

/// Fixed-point coefficients for one evaluation of the filter.
#[derive(Debug, Clone, Default)]
pub struct FilterCoefficients {
    /// Coefficients with 16 bits of fraction, indexed by `[section][tap]`.
    pub taps: [[i32; 8]; 2],
    /// Number of taps in use per section (twice the pair count).
    pub tap_counts: [usize; 2],
    /// Gain applied to the current input sample, with 16 bits of fraction.
    pub input_gain: i32,
}

impl SynthFilter {
    /// Decodes the filter description. If the filter's start and end configurations differ,
    /// breakpoints for `envelope` (which interpolates between them) follow in the stream.
    pub fn decode(data: &mut Buffer, envelope: &mut SynthEnvelope) -> Result<Self> {
        let mut filter = SynthFilter::default();
        let counts = data.get_u8()?;
        filter.pair_counts[FEEDFORWARD] = (counts >> 4) as usize;
        filter.pair_counts[FEEDBACK] = (counts & 0xf) as usize;
        if counts == 0 {
            return Ok(filter);
        }

        filter.attenuation[0] = data.get_u16()? as i32;
        filter.attenuation[1] = data.get_u16()? as i32;
        let end_differs_mask = data.get_u8()?;

        for section in 0..2 {
            for pair in 0..filter.pair_counts[section] {
                filter.frequencies[section][0][pair] = data.get_u16()? as i32;
                filter.magnitudes[section][0][pair] = data.get_u16()? as i32;
            }
        }
        for section in 0..2 {
            for pair in 0..filter.pair_counts[section] {
                if end_differs_mask & ((1 << (section * 4)) << pair) == 0 {
                    filter.frequencies[section][1][pair] = filter.frequencies[section][0][pair];
                    filter.magnitudes[section][1][pair] = filter.magnitudes[section][0][pair];
                } else {
                    filter.frequencies[section][1][pair] = data.get_u16()? as i32;
                    filter.magnitudes[section][1][pair] = data.get_u16()? as i32;
                }
            }
        }

        if end_differs_mask != 0 || filter.attenuation[1] != filter.attenuation[0] {
            envelope.decode_breakpoints(data)?;
        }
        Ok(filter)
    }

    /// Whether the filter does anything at all.
    pub fn is_active(&self) -> bool {
        self.pair_counts[FEEDFORWARD] > 0 || self.pair_counts[FEEDBACK] > 0
    }

    /// Computes coefficients for one section at envelope position `t` (0.0 = start
    /// configuration, 1.0 = end configuration), returning the number of taps. The feedforward
    /// section must be computed first, as it also sets the input gain.
    ///
    /// Each pair contributes the polynomial `1 - 2r cos(w) z^-1 + r^2 z^-2`; the pairs are
    /// multiplied together to produce the section's taps.
    pub fn compute(&self, section: usize, t: f32, out: &mut FilterCoefficients) -> usize {
        let mut float_taps = [0.0f32; 8];
        let mut input_gain = 0.0f32;

        if section == FEEDFORWARD {
            let attenuation =
                self.attenuation[0] as f32 + (self.attenuation[1] - self.attenuation[0]) as f32 * t;
            let attenuation = attenuation * 0.0030517578f32;
            input_gain = 0.1f64.powf((attenuation / 20.0f32) as f64) as f32;
            out.input_gain = (input_gain * 65536.0f32) as i32;
        }

        let pairs = self.pair_counts[section];
        out.tap_counts[section] = pairs * 2;
        if pairs == 0 {
            return 0;
        }

        let radius = self.pair_radius(section, 0, t);
        float_taps[0] = -2.0f32 * radius * (self.pair_phase(section, 0, t) as f64).cos() as f32;
        float_taps[1] = radius * radius;
        for pair in 1..pairs {
            let radius = self.pair_radius(section, pair, t);
            let b = -2.0f32 * radius * (self.pair_phase(section, pair, t) as f64).cos() as f32;
            let a = radius * radius;
            // Multiply the accumulated polynomial by (1 + b z^-1 + a z^-2).
            float_taps[pair * 2 + 1] = float_taps[pair * 2 - 1] * a;
            float_taps[pair * 2] = float_taps[pair * 2 - 1] * b + float_taps[pair * 2 - 2] * a;
            let mut i = pair * 2 - 1;
            while i >= 2 {
                float_taps[i] += float_taps[i - 1] * b + float_taps[i - 2] * a;
                i -= 1;
            }
            float_taps[1] += float_taps[0] * b + a;
            float_taps[0] += b;
        }

        if section == FEEDFORWARD {
            for tap in float_taps.iter_mut().take(pairs * 2) {
                *tap *= input_gain;
            }
        }
        for i in 0..pairs * 2 {
            out.taps[section][i] = (float_taps[i] * 65536.0f32) as i32;
        }
        pairs * 2
    }

    /// Distance of a pole/zero pair from the origin at envelope position `t`.
    fn pair_radius(&self, section: usize, pair: usize, t: f32) -> f32 {
        let start = self.magnitudes[section][0][pair];
        let end = self.magnitudes[section][1][pair];
        let magnitude = start as f32 + t * (end - start) as f32;
        let magnitude = magnitude * 0.0015258789f32;
        1.0f32 - 10.0f64.powf((-magnitude / 20.0f32) as f64) as f32
    }

    /// Angle of a pole/zero pair in radians per sample at envelope position `t`.
    fn pair_phase(&self, section: usize, pair: usize, t: f32) -> f32 {
        let start = self.frequencies[section][0][pair];
        let end = self.frequencies[section][1][pair];
        let octaves = start as f32 + t * (end - start) as f32;
        octave_to_phase(octaves * 1.2207031e-4f32)
    }
}

/// Converts a pitch in octaves above C1 (32.7 Hz) into radians per sample at 11025 Hz.
fn octave_to_phase(octaves: f32) -> f32 {
    let frequency = 2.0f64.powf(octaves as f64) as f32 * 32.703197f32;
    frequency * std::f32::consts::PI / 11025.0f32
}
