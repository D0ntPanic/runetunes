//! A single additive-synthesis tone, the building block of a [`SynthSound`](super::SynthSound).

use super::envelope::{SynthEnvelope, Waveform};
use super::filter::{FilterCoefficients, SynthFilter, FEEDBACK, FEEDFORWARD};
use crate::buffer::Buffer;
use anyhow::{anyhow, Result};
use std::sync::OnceLock;

/// Maximum number of harmonics (oscillators) in a tone.
const MAX_HARMONICS: usize = 5;

/// Oscillator phase is a 15-bit value (one full cycle is 32768 steps).
const PHASE_MASK: i32 = 0x7fff;

/// Phase-modulation or amplitude-modulation (tremolo/vibrato) settings.
#[derive(Debug, Clone)]
struct Modulation {
    /// Controls the modulation oscillator's rate (and selects its waveform).
    rate: SynthEnvelope,
    /// Controls the modulation depth over time.
    depth: SynthEnvelope,
}

/// Gate (on/off chopping) settings.
#[derive(Debug, Clone)]
struct Gate {
    /// Envelope selecting how long the tone stays silent, over time. Its value range is also
    /// used for the audible half of the cycle.
    closed_duration: SynthEnvelope,
    /// Envelope selecting how long the tone stays audible, over time.
    open_duration: SynthEnvelope,
}

/// One oscillator of a tone.
#[derive(Debug, Clone, Copy, Default)]
struct Harmonic {
    /// Volume as a percentage of the tone's amplitude envelope.
    volume: i32,
    /// Pitch offset relative to the tone's pitch envelope, in 1/10 semitones.
    pitch_offset: i32,
    /// Start delay in milliseconds.
    delay_ms: i32,
}

/// A single synthesized tone: up to five oscillators sharing pitch and amplitude envelopes,
/// with optional vibrato, tremolo, gating, a simple echo and a time-varying filter.
#[derive(Debug, Clone)]
pub struct SynthTone {
    /// Oscillator frequency over time; selects the oscillator waveform.
    pitch: SynthEnvelope,
    /// Amplitude over time.
    amplitude: SynthEnvelope,
    /// Optional pitch modulation (vibrato).
    vibrato: Option<Modulation>,
    /// Optional amplitude modulation (tremolo).
    tremolo: Option<Modulation>,
    /// Optional gating that periodically silences the tone.
    gate: Option<Gate>,
    /// Oscillators that make up the tone.
    harmonics: Vec<Harmonic>,
    /// Echo delay in milliseconds.
    echo_delay_ms: i32,
    /// Echo feedback volume as a percentage.
    echo_volume: i32,
    /// Duration of the tone in milliseconds.
    pub duration_ms: i32,
    /// Start time of the tone within the sound, in milliseconds.
    pub start_ms: i32,
    /// Time-varying filter applied to the oscillator output.
    filter: SynthFilter,
    /// Interpolates the filter between its start and end configurations.
    filter_envelope: SynthEnvelope,
}

impl SynthTone {
    /// Decodes a tone.
    pub fn decode(data: &mut Buffer) -> Result<Self> {
        let pitch = SynthEnvelope::decode(data)?;
        let amplitude = SynthEnvelope::decode(data)?;

        let vibrato = Self::decode_modulation(data)?;
        let tremolo = Self::decode_modulation(data)?;
        let gate = if data.peek_u8()? != 0 {
            Some(Gate {
                closed_duration: SynthEnvelope::decode(data)?,
                open_duration: SynthEnvelope::decode(data)?,
            })
        } else {
            data.get_u8()?;
            None
        };

        // Harmonic list is terminated by a zero volume.
        let mut harmonics = Vec::new();
        for _ in 0..10 {
            let volume = data.get_unsigned_smart()?;
            if volume == 0 {
                break;
            }
            if harmonics.len() >= MAX_HARMONICS {
                return Err(anyhow!("Too many harmonics in synth tone"));
            }
            let pitch_offset = data.get_signed_smart()?;
            let delay_ms = data.get_unsigned_smart()?;
            harmonics.push(Harmonic {
                volume,
                pitch_offset,
                delay_ms,
            });
        }

        let echo_delay_ms = data.get_unsigned_smart()?;
        let echo_volume = data.get_unsigned_smart()?;
        let duration_ms = data.get_u16()? as i32;
        let start_ms = data.get_u16()? as i32;
        let mut filter_envelope = SynthEnvelope::default();
        let filter = SynthFilter::decode(data, &mut filter_envelope)?;

        Ok(SynthTone {
            pitch,
            amplitude,
            vibrato,
            tremolo,
            gate,
            harmonics,
            echo_delay_ms,
            echo_volume,
            duration_ms,
            start_ms,
            filter,
            filter_envelope,
        })
    }

    fn decode_modulation(data: &mut Buffer) -> Result<Option<Modulation>> {
        if data.peek_u8()? != 0 {
            Ok(Some(Modulation {
                rate: SynthEnvelope::decode(data)?,
                depth: SynthEnvelope::decode(data)?,
            }))
        } else {
            data.get_u8()?;
            Ok(None)
        }
    }

    /// Renders the tone as `sample_count` samples of 16-bit range audio covering `duration_ms`.
    pub fn render(&mut self, sample_count: usize, duration_ms: i32) -> Vec<i32> {
        let mut samples = vec![0i32; sample_count];
        if duration_ms < 10 {
            return samples;
        }
        let n = sample_count as i32;
        let samples_per_ms = sample_count as f64 / (duration_ms as f64 + 0.0);

        self.pitch.reset();
        self.amplitude.reset();

        // Modulators run free-running oscillators whose rate is mapped from the rate envelope.
        let mut vibrato_step = 0;
        let mut vibrato_base_step = 0;
        let mut vibrato_phase = 0i32;
        if let Some(vibrato) = &mut self.vibrato {
            vibrato.rate.reset();
            vibrato.depth.reset();
            vibrato_step = ((vibrato
                .rate
                .end_value
                .wrapping_sub(vibrato.rate.start_value)) as f64
                * 32.768
                / samples_per_ms) as i32;
            vibrato_base_step = (vibrato.rate.start_value as f64 * 32.768 / samples_per_ms) as i32;
        }
        let mut tremolo_step = 0;
        let mut tremolo_base_step = 0;
        let mut tremolo_phase = 0i32;
        if let Some(tremolo) = &mut self.tremolo {
            tremolo.rate.reset();
            tremolo.depth.reset();
            tremolo_step = ((tremolo
                .rate
                .end_value
                .wrapping_sub(tremolo.rate.start_value)) as f64
                * 32.768
                / samples_per_ms) as i32;
            tremolo_base_step = (tremolo.rate.start_value as f64 * 32.768 / samples_per_ms) as i32;
        }

        // Per-harmonic oscillator state.
        let mut phases = [0i32; MAX_HARMONICS];
        let mut start_offsets = [0i32; MAX_HARMONICS];
        let mut amplitudes = [0i32; MAX_HARMONICS];
        let mut step_ranges = [0i32; MAX_HARMONICS];
        let mut base_steps = [0i32; MAX_HARMONICS];
        for (h, harmonic) in self.harmonics.iter().enumerate() {
            phases[h] = 0;
            start_offsets[h] = (harmonic.delay_ms as f64 * samples_per_ms) as i32;
            amplitudes[h] = (harmonic.volume << 14) / 100;
            // 1.0057929... is the 1/10 semitone frequency ratio (2^(1/120)).
            step_ranges[h] = ((self.pitch.end_value.wrapping_sub(self.pitch.start_value)) as f64
                * 32.768
                * 1.0057929410678534f64.powf(harmonic.pitch_offset as f64)
                / samples_per_ms) as i32;
            base_steps[h] = (self.pitch.start_value as f64 * 32.768 / samples_per_ms) as i32;
        }

        for i in 0..n {
            let mut pitch = self.pitch.next_level(n);
            let mut amplitude = self.amplitude.next_level(n);
            if let Some(vibrato) = &mut self.vibrato {
                let rate = vibrato.rate.next_level(n);
                let depth = vibrato.depth.next_level(n);
                pitch = pitch
                    .wrapping_add(oscillator(vibrato_phase, depth, vibrato.rate.waveform) >> 1);
                vibrato_phase = vibrato_phase
                    .wrapping_add((rate.wrapping_mul(vibrato_step) >> 16) + vibrato_base_step);
            }
            if let Some(tremolo) = &mut self.tremolo {
                let rate = tremolo.rate.next_level(n);
                let depth = tremolo.depth.next_level(n);
                amplitude = amplitude.wrapping_mul(
                    (oscillator(tremolo_phase, depth, tremolo.rate.waveform) >> 1) + 32768,
                ) >> 15;
                tremolo_phase = tremolo_phase
                    .wrapping_add((rate.wrapping_mul(tremolo_step) >> 16) + tremolo_base_step);
            }
            for h in 0..self.harmonics.len() {
                let index = i + start_offsets[h];
                if index < n {
                    let value = oscillator(
                        phases[h],
                        amplitude.wrapping_mul(amplitudes[h]) >> 15,
                        self.pitch.waveform,
                    );
                    samples[index as usize] = samples[index as usize].wrapping_add(value);
                    phases[h] = phases[h].wrapping_add(
                        (pitch.wrapping_mul(step_ranges[h]) >> 16).wrapping_add(base_steps[h]),
                    );
                }
            }
        }

        if let Some(gate) = &mut self.gate {
            gate.closed_duration.reset();
            gate.open_duration.reset();
            let mut counter = 0;
            let mut muted = true;
            let min = gate.closed_duration.start_value;
            let range = gate.closed_duration.end_value.wrapping_sub(min);
            for sample in samples.iter_mut() {
                let closed = gate.closed_duration.next_level(n);
                let open = gate.open_duration.next_level(n);
                let threshold = if muted {
                    min.wrapping_add(range.wrapping_mul(closed) >> 8)
                } else {
                    min.wrapping_add(range.wrapping_mul(open) >> 8)
                };
                counter += 256;
                if counter >= threshold {
                    counter = 0;
                    muted = !muted;
                }
                if muted {
                    *sample = 0;
                }
            }
        }

        if self.echo_delay_ms > 0 && self.echo_volume > 0 {
            let delay = (self.echo_delay_ms as f64 * samples_per_ms) as i32;
            for i in delay.max(0)..n {
                let echo = samples[(i - delay) as usize].wrapping_mul(self.echo_volume) / 100;
                samples[i as usize] = samples[i as usize].wrapping_add(echo);
            }
        }

        if self.filter.is_active() {
            self.apply_filter(&mut samples, n);
        }

        for sample in samples.iter_mut() {
            *sample = (*sample).clamp(-32768, 32767);
        }
        samples
    }

    /// Runs the tone through its filter in place. Coefficients are recomputed every 128 samples
    /// from the filter envelope.
    fn apply_filter(&mut self, samples: &mut [i32], n: i32) {
        let mut coefficients = FilterCoefficients::default();
        self.filter_envelope.reset();
        let mut level = self.filter_envelope.next_level(n + 1);
        let mut feedforward_taps =
            self.filter
                .compute(FEEDFORWARD, level as f32 / 65536.0f32, &mut coefficients)
                as i32;
        let mut feedback_taps =
            self.filter
                .compute(FEEDBACK, level as f32 / 65536.0f32, &mut coefficients) as i32;
        if n < feedforward_taps + feedback_taps {
            return;
        }

        let s = samples;
        let tap = |value: i32, coefficient: i32| -> i32 {
            ((value as i64 * coefficient as i64) >> 16) as i32
        };

        // The filter output overwrites the input in place. Output sample i depends on the
        // unfiltered inputs i..=i+feedforward_taps and on the previous filtered outputs.
        let mut i = 0i32;
        let mut limit = feedback_taps.min(n - feedforward_taps);
        // Warm-up: fewer than feedback_taps previous outputs exist yet.
        while i < limit {
            let mut value = tap(s[(i + feedforward_taps) as usize], coefficients.input_gain);
            for j in 0..feedforward_taps {
                value = value.wrapping_add(tap(
                    s[(i + feedforward_taps - j - 1) as usize],
                    coefficients.taps[FEEDFORWARD][j as usize],
                ));
            }
            for j in 0..i {
                value = value.wrapping_sub(tap(
                    s[(i - j - 1) as usize],
                    coefficients.taps[FEEDBACK][j as usize],
                ));
            }
            s[i as usize] = value;
            level = self.filter_envelope.next_level(n + 1);
            i += 1;
        }

        limit = 128;
        loop {
            if limit > n - feedforward_taps {
                limit = n - feedforward_taps;
            }
            while i < limit {
                let mut value = tap(s[(i + feedforward_taps) as usize], coefficients.input_gain);
                for j in 0..feedforward_taps {
                    value = value.wrapping_add(tap(
                        s[(i + feedforward_taps - j - 1) as usize],
                        coefficients.taps[FEEDFORWARD][j as usize],
                    ));
                }
                for j in 0..feedback_taps {
                    value = value.wrapping_sub(tap(
                        s[(i - j - 1) as usize],
                        coefficients.taps[FEEDBACK][j as usize],
                    ));
                }
                s[i as usize] = value;
                level = self.filter_envelope.next_level(n + 1);
                i += 1;
            }

            if i >= n - feedforward_taps {
                // Tail: the remaining samples have fewer future inputs available.
                while i < n {
                    let mut value = 0i32;
                    for j in (i + feedforward_taps - n)..feedforward_taps {
                        value = value.wrapping_add(tap(
                            s[(i + feedforward_taps - j - 1) as usize],
                            coefficients.taps[FEEDFORWARD][j as usize],
                        ));
                    }
                    for j in 0..feedback_taps {
                        value = value.wrapping_sub(tap(
                            s[(i - j - 1) as usize],
                            coefficients.taps[FEEDBACK][j as usize],
                        ));
                    }
                    s[i as usize] = value;
                    self.filter_envelope.next_level(n + 1);
                    i += 1;
                }
                break;
            }

            feedforward_taps =
                self.filter
                    .compute(FEEDFORWARD, level as f32 / 65536.0f32, &mut coefficients)
                    as i32;
            feedback_taps =
                self.filter
                    .compute(FEEDBACK, level as f32 / 65536.0f32, &mut coefficients)
                    as i32;
            limit += 128;
        }
    }
}

/// Evaluates an oscillator at `phase` (15-bit, wrapping) with the given amplitude.
fn oscillator(phase: i32, amplitude: i32, waveform: Waveform) -> i32 {
    match waveform {
        Waveform::Square => {
            if phase & PHASE_MASK < 16384 {
                amplitude
            } else {
                amplitude.wrapping_neg()
            }
        }
        Waveform::Sine => sine_table()[(phase & PHASE_MASK) as usize].wrapping_mul(amplitude) >> 14,
        Waveform::Sawtooth => ((phase & PHASE_MASK).wrapping_mul(amplitude) >> 14) - amplitude,
        Waveform::Noise => {
            noise_table()[((phase / 2607) & PHASE_MASK) as usize].wrapping_mul(amplitude)
        }
        Waveform::Off => 0,
    }
}

/// One cycle of a sine wave over 32768 entries, scaled to ±16384.
fn sine_table() -> &'static [i32; 32768] {
    static TABLE: OnceLock<Box<[i32; 32768]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = Box::new([0i32; 32768]);
        for (i, entry) in table.iter_mut().enumerate() {
            // 5215.1903 = 32768 / (2 pi)
            *entry = ((i as f64 / 5215.1903).sin() * 16384.0) as i32;
        }
        table
    })
}

/// 32768 entries of ±1 noise.
fn noise_table() -> &'static [i32; 32768] {
    static TABLE: OnceLock<Box<[i32; 32768]>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut random = JavaRandom::new(0);
        let mut table = Box::new([0i32; 32768]);
        for entry in table.iter_mut() {
            *entry = (random.next_int() & 2) - 1;
        }
        table
    })
}

/// Minimal reimplementation of `java.util.Random` (a 48-bit linear congruential generator).
struct JavaRandom {
    seed: i64,
}

impl JavaRandom {
    const MULTIPLIER: i64 = 0x5_deec_e66d;
    const MASK: i64 = (1 << 48) - 1;

    fn new(seed: i64) -> Self {
        JavaRandom {
            seed: (seed ^ Self::MULTIPLIER) & Self::MASK,
        }
    }

    fn next_int(&mut self) -> i32 {
        self.seed = (self.seed.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xb)) & Self::MASK;
        (self.seed >> 16) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::JavaRandom;

    /// `new java.util.Random(0).nextInt()` returns these values.
    #[test]
    fn java_random_matches_jdk() {
        let mut random = JavaRandom::new(0);
        assert_eq!(random.next_int(), -1155484576);
        assert_eq!(random.next_int(), -723955400);
        assert_eq!(random.next_int(), 1033096058);
    }
}
