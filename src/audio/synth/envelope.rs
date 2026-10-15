//! Piecewise-linear envelopes used by the procedural synthesizer.

use crate::buffer::Buffer;
use anyhow::Result;

/// Oscillator waveform selected by an envelope. Only meaningful for envelopes that drive an
/// oscillator (pitch envelopes and modulation rate envelopes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waveform {
    /// No oscillator output.
    Off,
    /// Square wave.
    Square,
    /// Sine wave (from a 32768-entry lookup table).
    Sine,
    /// Rising sawtooth.
    Sawtooth,
    /// White noise (from a 32768-entry table of ±1 values).
    Noise,
}

impl Waveform {
    fn from_id(id: u8) -> Self {
        match id {
            1 => Waveform::Square,
            2 => Waveform::Sine,
            3 => Waveform::Sawtooth,
            4 => Waveform::Noise,
            _ => Waveform::Off,
        }
    }
}

/// A piecewise-linear envelope over the duration of a tone.
///
/// The envelope is a list of `(time, level)` breakpoints. Times are fractions of the tone length
/// in units of 1/65536, and levels are unsigned 16-bit values. While generating a tone the
/// envelope is stepped once per output sample with [`SynthEnvelope::next_level`], which linearly
/// interpolates between breakpoints using 15 bits of fixed-point fraction.
///
/// Envelopes also carry two parameters whose meaning depends on how the envelope is used:
/// `start_value`/`end_value` give the range the envelope's level is mapped onto (for example the
/// starting and ending oscillator frequency of a pitch envelope), and `waveform` selects the
/// oscillator shape.
#[derive(Debug, Clone)]
pub struct SynthEnvelope {
    /// Oscillator waveform driven by this envelope.
    pub waveform: Waveform,
    /// Value the envelope maps to when its level is 0.
    pub start_value: i32,
    /// Value the envelope maps to when its level is 65535.
    pub end_value: i32,
    /// Breakpoint times, as fractions of the tone length (0..=65535).
    times: Vec<i32>,
    /// Breakpoint levels (0..=65535).
    levels: Vec<i32>,

    /// Index of the breakpoint currently being approached.
    stage: usize,
    /// Current level with 15 bits of fixed-point fraction.
    level: i32,
    /// Per-sample change in `level` for the current segment.
    slope: i32,
    /// Sample index at which the next breakpoint is reached.
    next_breakpoint_time: i32,
    /// Number of samples generated since the last [`SynthEnvelope::reset`].
    time: i32,
}

impl Default for SynthEnvelope {
    /// A linear ramp from 0 to 65535 over the full length of the tone.
    fn default() -> Self {
        SynthEnvelope {
            waveform: Waveform::Off,
            start_value: 0,
            end_value: 0,
            times: vec![0, 65535],
            levels: vec![0, 65535],
            stage: 0,
            level: 0,
            slope: 0,
            next_breakpoint_time: 0,
            time: 0,
        }
    }
}

impl SynthEnvelope {
    /// Decodes an envelope with its waveform, value range and breakpoints.
    pub fn decode(data: &mut Buffer) -> Result<Self> {
        let mut envelope = SynthEnvelope {
            waveform: Waveform::from_id(data.get_u8()?),
            start_value: data.get_i32()?,
            end_value: data.get_i32()?,
            ..SynthEnvelope::default()
        };
        envelope.decode_breakpoints(data)?;
        Ok(envelope)
    }

    /// Decodes only the breakpoint list, replacing the current breakpoints.
    pub fn decode_breakpoints(&mut self, data: &mut Buffer) -> Result<()> {
        let count = data.get_u8()? as usize;
        self.times = Vec::with_capacity(count);
        self.levels = Vec::with_capacity(count);
        for _ in 0..count {
            self.times.push(data.get_u16()? as i32);
            self.levels.push(data.get_u16()? as i32);
        }
        Ok(())
    }

    /// Rewinds the envelope to the start of a tone.
    pub fn reset(&mut self) {
        self.next_breakpoint_time = 0;
        self.stage = 0;
        self.slope = 0;
        self.level = 0;
        self.time = 0;
    }

    /// Advances the envelope by one sample of a tone that is `tone_samples` samples long and
    /// returns the level for the current sample (0..=65535).
    pub fn next_level(&mut self, tone_samples: i32) -> i32 {
        if self.time >= self.next_breakpoint_time {
            // Arrived at a breakpoint: snap to its level and aim for the next one.
            self.level = self.levels[self.stage] << 15;
            self.stage += 1;
            if self.stage >= self.levels.len() {
                self.stage = self.levels.len() - 1;
            }
            self.next_breakpoint_time =
                (self.times[self.stage] as f64 / 65536.0 * tone_samples as f64) as i32;
            if self.next_breakpoint_time > self.time {
                self.slope = ((self.levels[self.stage] << 15).wrapping_sub(self.level))
                    / (self.next_breakpoint_time - self.time);
            }
        }
        self.level = self.level.wrapping_add(self.slope);
        self.time += 1;
        self.level.wrapping_sub(self.slope) >> 15
    }
}
