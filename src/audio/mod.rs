//! An implementation of the RuneScape music engine in two versions: RuneScape 2009 (build 530)
//! and Old School RuneScape. [`Engine`] selects between them and lists how they differ.
//!
//! The client plays music with its own sample-based MIDI synthesizer. This module reproduces
//! it closely enough to produce bit-identical output (verified against the original Java code
//! of both clients).
//!
//! # Data
//!
//! * **Songs** (archive 6) are MIDI sequences in a compact column-oriented encoding, parsed by
//!   [`crate::song::Song`].
//! * **Patches** (archive 15, [`patch::Patch`]) are the instruments, indexed by MIDI program
//!   number with the bank select bits added (bank `b`, program `p` is patch `b * 128 + p`;
//!   channel 10 defaults to bank 1, so drum kits are patches 128 and up). A patch maps each key
//!   to a sample plus tuning, volume, pan, an exclusive class and an [`patch::Articulation`]
//!   (attack/release envelopes, decay and vibrato).
//! * **Samples** are 8-bit (build 530) or 16-bit (Old School) mono PCM with loop points
//!   ([`sample::Sample`]), decoded either from Jagex's stripped-down Vorbis streams in
//!   archive 14 ([`vorbis`]) or rendered from the procedural synth sounds in archive 4
//!   ([`synth`]). [`sample_bank::SampleBank`] loads and shares them.
//!
//! # Playback pipeline
//!
//! 1. [`midi_synth::MidiSynth`] sequences the song, tracks MIDI channel state (volume,
//!    expression, pan, pitch bend, sustain, portamento, ...) and owns the sounding notes. Every
//!    10 ms "control tick" each note's pitch and envelopes are updated.
//! 2. Each note plays its sample through a [`voice::SampleVoice`], which resamples with linear
//!    interpolation, handles loops and ramps volume/pan smoothly between control ticks.
//! 3. [`renderer::MusicRenderer`] renders 256 or 512-frame chunks into a 32-bit mix buffer,
//!    periodically runs a 32-voice limiter, and converts to 16-bit output.
//! 4. [`device`] streams the output to a sound card with `cpal`.
//!
//! All arithmetic is integer fixed-point except a few places (pitch to playback rate
//! conversion, envelope scaling, panning, the synth filter and the Vorbis decoder) that use
//! floating point in exactly the same order as the original client.

#![allow(clippy::needless_range_loop)]

pub mod device;
pub mod midi_synth;
pub mod patch;
pub mod renderer;
pub mod sample;
pub mod sample_bank;
pub mod synth;
pub mod voice;
pub mod vorbis;

use crate::data::Data;
use crate::song::Song;
use anyhow::Result;
use midi_synth::MidiSynth;
use renderer::MusicRenderer;

/// Which version of the client's music engine to reproduce.
///
/// Old School RuneScape's client descends from a 2007 snapshot of the same code, which Jagex
/// later reworked for 16-bit audio. Both play the same song and patch formats; the engine
/// differs in these ways:
///
/// | | Build 530 | Old School |
/// | --- | --- | --- |
/// | Sample data | 8-bit | 16-bit (Vorbis decoded at full precision, synth sounds mixed in 16 bits) |
/// | Synth sound sample rate | output rate | always 22050 Hz |
/// | Stereo mixing | 8-bit kernels | 16-bit kernels (mono still uses the 8-bit ones) |
/// | Output chunk size | 256 frames | 512 frames |
/// | Decay (articulation) | ends the note once it falls below the cutoff | never ends the note |
/// | Voice limiter priorities | synth at the root | synth inside a mixer, one level deeper |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// RuneScape 2, build 530 (2009scape).
    Build530,
    /// Old School RuneScape.
    Osrs,
}

impl Engine {
    /// Frames the output stage mixes per chunk.
    pub fn chunk_frames(self) -> usize {
        match self {
            Engine::Build530 => 256,
            Engine::Osrs => 512,
        }
    }

    /// Whether instrument samples are 16-bit (otherwise 8-bit values are stored in the same
    /// 16-bit containers).
    pub fn sixteen_bit_samples(self) -> bool {
        self == Engine::Osrs
    }

    /// Rate synth sounds are rendered at for a given output rate.
    pub fn synth_sample_rate(self, output_rate: i32) -> i32 {
        match self {
            Engine::Build530 => output_rate,
            Engine::Osrs => 22050,
        }
    }

    /// Whether stereo voices use the 16-bit mixing kernels. Old School's mono kernels were not
    /// updated and still treat the samples as 8-bit, so its mono output is about 256 times too
    /// loud and mostly clipped. This differs from that implementation so that mono mode can
    /// actually be used.
    pub fn wide_mixing(self) -> bool {
        self == Engine::Osrs
    }

    /// Whether a note ends once its exponential decay has run its course.
    pub fn decay_ends_notes(self) -> bool {
        self == Engine::Build530
    }

    /// Number of streams above the note list in the stream tree, each of which scales voice
    /// priorities by 255/256 in the voice limiter.
    pub fn stream_depth(self) -> usize {
        match self {
            // AudioChannel -> MidiPcmStream -> MidiNoteStream
            Engine::Build530 => 1,
            // PcmPlayer -> PcmStreamMixer -> MidiPcmStream -> MusicPatchPcmStream
            Engine::Osrs => 2,
        }
    }
}

/// User-selectable playback settings.
#[derive(Debug, Clone)]
pub struct PlaybackOptions {
    /// Output (and synth sound rendering) sample rate in Hz. The default is 22050.
    pub sample_rate: i32,
    /// Stereo or mono output.
    pub stereo: bool,
    /// Music volume, 0..=255.
    pub volume: i32,
    /// Restart the song when it ends.
    pub looping: bool,
}

impl MusicRenderer {
    /// Loads a song's patches and samples and starts playing it.
    pub fn for_song(data: &Data, song: &Song, options: &PlaybackOptions) -> Result<Self> {
        let mut synth = MidiSynth::new(data.engine, options.sample_rate, options.stereo);
        synth.load_patches(data, song)?;
        synth.set_volume(options.volume);
        synth.play(song, options.looping);
        Ok(MusicRenderer::new(
            data.engine,
            synth,
            options.sample_rate,
            options.stereo,
        ))
    }
}
