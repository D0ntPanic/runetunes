//! Decoded instrument samples.

/// A decoded mono sample with optional loop points. Every instrument key in a [`Patch`] plays
/// one of these, resampled to the note's pitch by a [`SampleVoice`].
///
/// Samples come from either the procedural synthesizer ([`SynthSound`]) or the Vorbis sample
/// archive ([`VorbisSample`]). Build 530 decodes them to signed 8-bit PCM, Old School to signed
/// 16-bit PCM; both are stored here as `i16` (8-bit samples keep their -128..=127 range).
///
/// [`Patch`]: super::patch::Patch
/// [`SampleVoice`]: super::voice::SampleVoice
/// [`SynthSound`]: super::synth::SynthSound
/// [`VorbisSample`]: super::vorbis::VorbisSample
#[derive(Debug, Clone)]
pub struct Sample {
    /// Native sample rate of `data`, in Hz.
    pub sample_rate: i32,
    /// Signed PCM sample data, 8-bit or 16-bit depending on the engine.
    pub data: Vec<i16>,
    /// Index of the first sample in the loop region.
    pub loop_start: i32,
    /// Index one past the last sample of the loop region. When `loop_end <= loop_start` the
    /// sample has no usable loop and will play through to the end once.
    pub loop_end: i32,
    /// When set, the loop region is played alternately forwards and backwards ("ping-pong"
    /// looping) instead of jumping back to `loop_start`.
    pub ping_pong: bool,
}

impl Sample {
    /// Sample length in samples.
    pub fn len(&self) -> i32 {
        self.data.len() as i32
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}
