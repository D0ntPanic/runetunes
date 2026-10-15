//! Procedural synthesizer for the sounds stored in cache archive 4.
//!
//! Archive 4 holds Jagex's "synth" sounds: instead of recorded audio, each sound is a short
//! program of up to ten [`SynthTone`]s. Each tone is an additive oscillator bank shaped by
//! piecewise-linear envelopes ([`envelope::SynthEnvelope`]), with optional vibrato, tremolo,
//! gating, echo and a time-varying IIR filter ([`filter::SynthFilter`]). The same format is
//! used for game sound effects; the music engine uses it for some instrument samples.

pub mod envelope;
pub mod filter;
pub mod tone;

pub use tone::SynthTone;

use super::sample::Sample;
use crate::buffer::Buffer;
use anyhow::{anyhow, Result};

/// Maximum number of tones in a synth sound.
const MAX_TONES: usize = 10;

/// A synthesized sound made of up to ten tones mixed together.
#[derive(Debug, Clone)]
pub struct SynthSound {
    /// Tone slots; empty slots are `None`.
    tones: Vec<Option<SynthTone>>,
    /// Start of the loop region in milliseconds.
    loop_start_ms: i32,
    /// End of the loop region in milliseconds.
    loop_end_ms: i32,
}

impl SynthSound {
    /// Decodes a synth sound from its cache file.
    pub fn decode(data: Vec<u8>) -> Result<Self> {
        let mut data = Buffer::new(data);
        let mut tones = Vec::with_capacity(MAX_TONES);
        for _ in 0..MAX_TONES {
            if data.peek_u8()? != 0 {
                tones.push(Some(SynthTone::decode(&mut data)?));
            } else {
                data.get_u8()?;
                tones.push(None);
            }
        }
        let loop_start_ms = data.get_u16()? as i32;
        let loop_end_ms = data.get_u16()? as i32;
        Ok(SynthSound {
            tones,
            loop_start_ms,
            loop_end_ms,
        })
    }

    /// Renders the sound to signed 8-bit or 16-bit PCM at `sample_rate`.
    fn render_samples(&mut self, sample_rate: i32, sixteen_bit: bool) -> Result<Vec<i16>> {
        let duration_ms = self
            .tones
            .iter()
            .flatten()
            .map(|tone| tone.duration_ms + tone.start_ms)
            .max()
            .unwrap_or(0)
            .max(0);
        if duration_ms == 0 {
            return Ok(Vec::new());
        }

        let total_samples = duration_ms.wrapping_mul(sample_rate) / 1000;
        if total_samples < 0 {
            return Err(anyhow!("Synth sound too long"));
        }
        let mut output = vec![0i16; total_samples as usize];
        for tone in self.tones.iter_mut().flatten() {
            let tone_samples = tone.duration_ms.wrapping_mul(sample_rate) / 1000;
            let start = (tone.start_ms.wrapping_mul(sample_rate) / 1000) as usize;
            let rendered = tone.render(tone_samples.max(0) as usize, tone.duration_ms);
            for (i, value) in rendered.iter().enumerate() {
                // Tones are rendered at 16-bit resolution and mixed with saturation, either at
                // full resolution or reduced to 8 bits.
                let current = output[i + start] as i32;
                output[i + start] = if sixteen_bit {
                    (current + value).clamp(-32768, 32767) as i16
                } else {
                    (current + (value >> 8)).clamp(-128, 127) as i16
                };
            }
        }
        Ok(output)
    }

    /// Renders the sound into an 8-bit or 16-bit [`Sample`] at `sample_rate`.
    pub fn render(&mut self, sample_rate: i32, sixteen_bit: bool) -> Result<Sample> {
        let data = self.render_samples(sample_rate, sixteen_bit)?;
        Ok(Sample {
            sample_rate,
            data,
            loop_start: self.loop_start_ms.wrapping_mul(sample_rate) / 1000,
            loop_end: self.loop_end_ms.wrapping_mul(sample_rate) / 1000,
            ping_pong: false,
        })
    }
}
