//! The MIDI synthesizer: sequencing, channel state and note lifecycle.

use super::patch::{Articulation, Patch, KEY_COUNT};
use super::sample::Sample;
use super::sample_bank::SampleBank;
use super::voice::{MixFormat, SampleVoice, PAN_CENTER};
use super::Engine;
use crate::data::Data;
use crate::song::{Event, Song, TrackEvent};
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Number of MIDI channels.
const CHANNEL_COUNT: usize = 16;

/// MIDI channel 10, which defaults to the percussion bank.
const PERCUSSION_CHANNEL: usize = 9;

/// Patch selected by default on the percussion channel (bank 1, program 0).
const PERCUSSION_PATCH: i32 = 128;

/// Tempo before the first tempo event, in microseconds per quarter note (120 BPM).
const DEFAULT_TEMPO: i32 = 500_000;

/// Neutral value of 14-bit controllers such as pan and pitch bend.
const CONTROLLER_CENTER: i32 = 8192;

/// Value of the registered parameter number selecting pitch bend range (RPN 0, offset by
/// 16384 to distinguish it from non-registered parameters).
const RPN_PITCH_BEND_RANGE: i32 = 16384;

/// Value of the selected parameter number when none is selected.
const PARAMETER_NONE: i32 = 32767;

/// Envelope position increment per control tick.
const ENVELOPE_STEP: i32 = 128;

/// Retrigger phase wraps at 2^20.
const RETRIGGER_PHASE_ONE: i32 = 1 << 20;

/// Portamento progress at the start of a glide (it counts down to 0).
const PORTAMENTO_START: i32 = 4096;

/// State of one MIDI channel.
#[derive(Debug, Clone)]
struct ChannelState {
    /// Channel volume, CC 7/39 (14-bit).
    volume: i32,
    /// Channel pan, CC 10/42 (14-bit, 8192 = centre).
    pan: i32,
    /// Expression, CC 11/43 (14-bit).
    expression: i32,
    /// Pitch wheel position (14-bit, 8192 = centre).
    pitch_bend: i32,
    /// Pitch bend range in 1/128 semitones (so the data entry MSB is whole semitones). The pitch
    /// offset in 1/256 semitones is `range * (bend - 8192) >> 12`; the default of 256 gives
    /// ±2 semitones. Set through RPN 0 data entry.
    pitch_bend_range: i32,
    /// Modulation wheel, CC 1/33 (14-bit). Adds `modulation >> 7` (MSB 127: about half a
    /// semitone) to the vibrato depth of notes whose articulation has vibrato.
    modulation: i32,
    /// Portamento speed, CC 5/37 (14-bit). Each tick the glide progresses by
    /// `16 * 2^(value / 2032)` out of 4096, so higher values glide faster (the default 8192
    /// glides in about 160 ms).
    portamento_time: i32,
    /// Selected registered/non-registered parameter for data entry, CC 98-101.
    parameter_number: i32,
    /// Sample start offset, CC 16/48 (General Purpose 1): notes start this far into their
    /// sample, as a fraction of the sample length (16384 = the end).
    sample_offset: i32,
    /// Raw retrigger rate, CC 17/49 (General Purpose 2).
    retrigger_rate_setting: i32,
    /// Retrigger phase increment per second, derived from `retrigger_rate_setting`.
    retrigger_rate: i32,
    /// Sustain pedal, CC 64: holds release envelopes.
    sustain: bool,
    /// Portamento, CC 65: new notes glide from the held note instead of starting fresh.
    portamento: bool,
    /// Retrigger, CC 81 (General Purpose 6): held notes restart their sample periodically at
    /// `retrigger_rate`.
    retrigger: bool,
    /// Selected patch (bank and program).
    patch: i32,
    /// Bank select bits, CC 0/32, added to program changes.
    bank: i32,
    /// Patch selected when the channel is reset.
    default_patch: i32,
    /// Additional channel gain, 256 = unity.
    gain: i32,
}

impl ChannelState {
    fn new() -> Self {
        let mut channel = ChannelState {
            volume: 0,
            pan: 0,
            expression: 0,
            pitch_bend: 0,
            pitch_bend_range: 0,
            modulation: 0,
            portamento_time: 0,
            parameter_number: 0,
            sample_offset: 0,
            retrigger_rate_setting: 0,
            retrigger_rate: 0,
            sustain: false,
            portamento: false,
            retrigger: false,
            patch: 0,
            bank: 0,
            default_patch: 0,
            gain: 256,
        };
        channel.reset_controllers_values();
        channel
    }

    fn reset_controllers_values(&mut self) {
        self.volume = 12800;
        self.pan = CONTROLLER_CENTER;
        self.expression = 16383;
        self.pitch_bend = CONTROLLER_CENTER;
        self.modulation = 0;
        self.portamento_time = CONTROLLER_CENTER;
        self.sustain = false;
        self.portamento = false;
        self.retrigger = false;
        self.parameter_number = PARAMETER_NONE;
        self.pitch_bend_range = 256;
        self.sample_offset = 0;
        self.set_retrigger_rate(CONTROLLER_CENTER);
    }

    /// Sets the retrigger rate from its controller value. The phase increment per second is
    /// 2^21 * 2^(value / 1820.4), i.e. 2 retriggers per second at 0, doubling every ~1820 steps.
    fn set_retrigger_rate(&mut self, setting: i32) {
        self.retrigger_rate_setting = setting;
        self.retrigger_rate =
            (2.0f64.powf(setting as f64 * 5.4931640625e-4) * 2097152.0 + 0.5) as i32;
    }
}

/// A sounding (or held) note.
///
/// Notes are shared between the active note list and the per-channel lookup tables, and those
/// tables can keep referring to a note after it leaves the list.
#[derive(Debug)]
struct ActiveNote {
    /// MIDI channel.
    channel: usize,
    /// MIDI key (changes during portamento).
    key: i32,
    /// Patch the note was started with.
    patch: Option<Rc<Patch>>,
    /// Sample being played.
    sample: Option<Rc<Sample>>,
    /// Envelope and vibrato settings.
    articulation: Option<Rc<Articulation>>,
    /// The voice rendering the sample; `None` once the note has finished.
    voice: Option<SampleVoice>,
    /// Exclusive class from the patch, or -1.
    exclusive_class: i32,
    /// Note volume from velocity and the patch's key and patch volumes.
    volume: i32,
    /// Note pan from the patch, 0..=128.
    pan: i32,
    /// Pitch relative to the sample's root, in 1/256 semitones.
    pitch: i32,
    /// Pitch offset at the start of a portamento glide, in 1/256 semitones.
    portamento_offset: i32,
    /// Glide progress, from 4096 down to 0; the applied offset is
    /// `portamento_offset * portamento_progress >> 12`.
    portamento_progress: i32,
    /// -1 while the key is held; after release, the position in the release envelope.
    release_position: i32,
    /// Position in the attack envelope.
    attack_position: i32,
    /// Current segment of the attack envelope.
    attack_index: usize,
    /// Current segment of the release envelope.
    release_index: usize,
    /// Progress of the exponential decay.
    decay_position: i32,
    /// Vibrato phase (512 per cycle).
    vibrato_phase: i32,
    /// Control ticks since the note started, for vibrato fade-in.
    ticks: i32,
    /// Output samples until the next control tick.
    samples_until_tick: i32,
    /// Retrigger phase accumulator, wrapping at 2^20.
    retrigger_phase: i32,
}

impl ActiveNote {
    fn is_held(&self) -> bool {
        self.release_position < 0
    }

    /// Drops the note's sample and voice once it has finished sounding.
    fn release_resources(&mut self) {
        self.articulation = None;
        self.sample = None;
        self.voice = None;
        self.patch = None;
    }
}

type NoteRef = Rc<RefCell<ActiveNote>>;

/// Song playback position.
///
/// The original client re-encodes each song to a standard MIDI file and plays it with a
/// small MIDI file reader that interleaves the tracks by picking the track with the earliest
/// next event (lowest track index on ties). Walking the song's merged event list, which is
/// sorted the same way, is equivalent.
///
/// Time is tracked in "microsecond-ticks": microseconds × ticks per quarter note, so the time
/// of tick `t` is `start_time + t * tempo` without any division.
#[derive(Debug, Clone)]
struct Sequencer {
    events: Rc<[TrackEvent]>,
    /// Index of the next event to process.
    index: usize,
    /// Number of tracks in the song.
    track_count: usize,
    /// Tracks that have not reached their end-of-track event.
    tracks_remaining: usize,
    /// Ticks per quarter note.
    division: i32,
    /// Time of tick 0 (adjusted by tempo changes so the mapping stays continuous).
    start_time: i64,
    /// Current tempo in microseconds per quarter note.
    tempo: i32,
}

impl Sequencer {
    fn new(song: &Song) -> Self {
        Sequencer {
            events: song.events.clone().into(),
            index: 0,
            track_count: song.tracks.len(),
            tracks_remaining: song.tracks.len(),
            division: song.division as i32,
            start_time: 0,
            tempo: DEFAULT_TEMPO,
        }
    }

    /// Tick of the next event, if any.
    fn next_tick(&self) -> Option<i32> {
        self.events.get(self.index).map(|event| event.time as i32)
    }

    /// Time of a tick in microsecond-ticks.
    fn time_of(&self, tick: i32) -> i64 {
        self.start_time + tick as i64 * self.tempo as i64
    }

    /// Restarts from the first event, with tick 0 at `time`.
    fn restart(&mut self, time: i64) {
        self.index = 0;
        self.tracks_remaining = self.track_count;
        self.start_time = time;
    }
}

/// Plays a [`Song`] by rendering every note through a [`SampleVoice`].
///
/// Rendering is driven by [`MidiSynth::read`]. Output is produced in segments: the synth renders
/// all notes up to the next MIDI event, processes every event at that tick, and continues.
///
/// Each note is updated on a fixed 10 ms "control tick" (`sample_rate / 100` samples). On each
/// tick the note's pitch is recomputed (pitch bend, portamento glide, vibrato) and its envelopes
/// are advanced (attack envelope, exponential decay, and after note-off the release envelope).
/// The voice then ramps linearly to the new volume and pan over the following tick, so all
/// parameter changes are smooth.
///
/// When a note ends its voice fades out over one tick. Fading voices are moved to a separate
/// list (`fading_voices`) so they can keep playing after the note is gone.
pub struct MidiSynth {
    engine: Engine,
    /// Output sample rate in Hz.
    sample_rate: i32,
    /// Output layout and mixing kernels.
    format: MixFormat,

    /// Playback position, or `None` when no song is playing.
    sequencer: Option<Sequencer>,
    /// Whether the song restarts when it ends.
    looping: bool,
    /// Current output time in microsecond-ticks.
    current_time: i64,
    /// Time of the next event in microsecond-ticks.
    next_event_time: i64,
    /// Tick of the next event.
    next_event_tick: i32,

    /// Overall music volume, 0..=256.
    master_volume: i32,
    channels: Vec<ChannelState>,
    /// Loaded patches by patch number.
    patches: HashMap<i32, Rc<Patch>>,

    /// Notes in start order.
    notes: Vec<NoteRef>,
    /// The note held on each channel and key, for note-off lookups.
    held_notes: Vec<[Option<NoteRef>; KEY_COUNT]>,
    /// The most recent note of each exclusive class on each channel.
    exclusive_notes: Vec<[Option<NoteRef>; KEY_COUNT]>,
    /// Voices fading out after their note ended.
    fading_voices: Vec<SampleVoice>,
}

impl MidiSynth {
    /// Creates a synthesizer in its power-on state.
    pub fn new(engine: Engine, sample_rate: i32, stereo: bool) -> Self {
        let mut synth = MidiSynth {
            engine,
            sample_rate,
            format: MixFormat {
                stereo,
                wide: engine.wide_mixing(),
            },
            sequencer: None,
            looping: false,
            current_time: 0,
            next_event_time: 0,
            next_event_tick: 0,
            master_volume: 256,
            channels: (0..CHANNEL_COUNT).map(|_| ChannelState::new()).collect(),
            patches: HashMap::new(),
            notes: Vec::new(),
            held_notes: (0..CHANNEL_COUNT)
                .map(|_| std::array::from_fn(|_| None))
                .collect(),
            exclusive_notes: (0..CHANNEL_COUNT)
                .map(|_| std::array::from_fn(|_| None))
                .collect(),
            fading_voices: Vec::new(),
        };
        synth.reset(true);
        synth.channels[PERCUSSION_CHANNEL].default_patch = PERCUSSION_PATCH;
        synth.channels[PERCUSSION_CHANNEL].bank = PERCUSSION_PATCH;
        synth.select_patch(PERCUSSION_CHANNEL, PERCUSSION_PATCH);
        synth
    }

    /// Samples per control tick (10 ms).
    fn tick_samples(&self) -> i32 {
        self.sample_rate / 100
    }

    /// Loads the patches and samples a song needs, replacing any previously loaded patches.
    pub fn load_patches(&mut self, data: &Data, song: &Song) -> Result<()> {
        self.patches.clear();
        let mut bank = SampleBank::new(data, self.sample_rate);
        for (patch_id, used_keys) in song.patch_usage() {
            let file = data
                .patch_archive
                .get_by_id(patch_id as usize)
                .with_context(|| {
                    format!(
                        "UNPLAYABLE: song uses patch {} which is missing from the cache",
                        patch_id
                    )
                })?;
            let mut patch =
                Patch::decode(file).with_context(|| format!("Decoding patch {}", patch_id))?;
            patch.load_samples(&mut bank, Some(&used_keys))?;
            self.patches.insert(patch_id as i32, Rc::new(patch));
        }
        Ok(())
    }

    /// Sets the overall music volume (0..=256).
    pub fn set_volume(&mut self, volume: i32) {
        self.master_volume = volume;
    }

    /// Starts playing a song from the beginning, stopping anything already playing.
    /// The song's patches must have been loaded with [`MidiSynth::load_patches`].
    pub fn play(&mut self, song: &Song, looping: bool) {
        self.stop_sequence(true);
        let sequencer = Sequencer::new(song);
        self.looping = looping;
        self.current_time = 0;
        self.next_event_tick = sequencer.next_tick().unwrap_or(0);
        self.next_event_time = sequencer.time_of(self.next_event_tick);
        self.sequencer = Some(sequencer);
    }

    /// Whether a song is still being sequenced.
    pub fn is_playing(&self) -> bool {
        self.sequencer.is_some()
    }

    /// Whether anything is still audible (a song is playing, or notes are still sounding).
    pub fn is_active(&self) -> bool {
        self.sequencer.is_some() || !self.notes.is_empty() || !self.fading_voices.is_empty()
    }

    /// Stops sequencing and resets all channels.
    fn stop_sequence(&mut self, kill_notes: bool) {
        self.sequencer = None;
        self.reset(kill_notes);
    }

    /// Silences notes (abruptly if `kill_notes`, otherwise by releasing them), resets all
    /// controllers and restores each channel's default patch.
    fn reset(&mut self, kill_notes: bool) {
        if kill_notes {
            self.all_sound_off(None);
        } else {
            self.all_notes_off(None);
        }
        self.reset_controllers(None);
        for channel in self.channels.iter_mut() {
            channel.patch = channel.default_patch;
            channel.bank = channel.default_patch & !0x7f;
        }
    }

    /// Mixes `count` frames into `out` starting at frame `offset`, advancing the song.
    pub fn read(&mut self, out: &mut [i32], mut offset: i32, mut count: i32) {
        if let Some(sequencer) = &self.sequencer {
            // Song time advanced per output sample, in microsecond-ticks.
            let time_per_sample =
                (sequencer.division.wrapping_mul(1_000_000) / self.sample_rate) as i64;
            loop {
                let target = self.current_time + count as i64 * time_per_sample;
                if self.next_event_time - target >= 0 {
                    self.current_time = target;
                    break;
                }
                // Render up to the first sample at or after the next event.
                let samples = ((self.next_event_time + time_per_sample - self.current_time - 1)
                    / time_per_sample) as i32;
                self.current_time += time_per_sample * samples as i64;
                self.render_notes(out, offset, samples);
                count -= samples;
                offset += samples;
                self.process_events();
                if self.sequencer.is_none() {
                    break;
                }
            }
        }
        self.render_notes(out, offset, count);
    }

    /// Processes every event at the next tick and schedules the following one.
    fn process_events(&mut self) {
        let Some(mut sequencer) = self.sequencer.take() else {
            return;
        };
        let current_tick = self.next_event_tick;
        let mut tick = current_tick;
        let mut time = self.next_event_time;

        while tick == current_tick {
            while sequencer.next_tick() == Some(tick) {
                let event = sequencer.events[sequencer.index].event.clone();
                sequencer.index += 1;
                match event {
                    Event::EndOfTrack => {
                        sequencer.tracks_remaining -= 1;
                        if sequencer.tracks_remaining == 0 {
                            if !self.looping || tick == 0 {
                                self.reset(true);
                                return;
                            }
                            sequencer.restart(time);
                        }
                        break;
                    }
                    Event::TempoChange(tempo) => {
                        // Keep the time of the current tick unchanged under the new tempo.
                        let tempo = tempo as i32;
                        sequencer.start_time +=
                            tick as i64 * (sequencer.tempo as i64 - tempo as i64);
                        sequencer.tempo = tempo;
                    }
                    event => self.handle_event(event),
                }
            }
            // A finished song with tracks that never ended has no next event.
            tick = sequencer.next_tick().unwrap_or(i32::MAX);
            time = sequencer.time_of(tick);
        }

        self.next_event_tick = tick;
        self.next_event_time = time;
        self.sequencer = Some(sequencer);
    }

    /// Dispatches a channel event.
    fn handle_event(&mut self, event: Event) {
        match event {
            Event::NoteOff { channel, note, .. } => self.note_off(channel as usize, note as i32),
            Event::NoteOn {
                channel,
                note,
                velocity,
            } => {
                if velocity > 0 {
                    self.note_on(channel as usize, note as i32, velocity as i32);
                } else {
                    self.note_off(channel as usize, note as i32);
                }
            }
            Event::Controller {
                channel,
                controller,
                value,
            } => self.control_change(channel as usize, controller, value as i32),
            Event::ProgramChange { channel, program }
            | Event::ProgramSelect {
                channel, program, ..
            } => {
                let channel = channel as usize;
                let patch = self.channels[channel].bank + program as i32;
                self.select_patch(channel, patch);
            }
            Event::PitchWheel { channel, position } => {
                self.channels[channel as usize].pitch_bend = position as i32;
            }
            // Aftertouch is ignored by the original client.
            Event::KeyPressure { .. } | Event::ChannelPressure { .. } => {}
            Event::EndOfTrack | Event::TempoChange(_) => {}
        }
    }

    /// Selects a patch on a channel, clearing the exclusive class slots when it changes.
    fn select_patch(&mut self, channel: usize, patch: i32) {
        if self.channels[channel].patch != patch {
            self.channels[channel].patch = patch;
            self.exclusive_notes[channel] = std::array::from_fn(|_| None);
        }
    }

    /// Handles a control change.
    fn control_change(&mut self, channel: usize, controller: u8, value: i32) {
        let state = &mut self.channels[channel];
        let msb = |current: i32| (current & !0x3f80) + (value << 7);
        let lsb = |current: i32| (current & !0x7f) + value;
        match controller {
            0 => state.bank = (state.bank & !(0x7f << 14)) + (value << 14),
            32 => state.bank = msb(state.bank),
            1 => state.modulation = msb(state.modulation),
            33 => state.modulation = lsb(state.modulation),
            5 => state.portamento_time = msb(state.portamento_time),
            37 => state.portamento_time = lsb(state.portamento_time),
            7 => state.volume = msb(state.volume),
            39 => state.volume = lsb(state.volume),
            10 => state.pan = msb(state.pan),
            42 => state.pan = lsb(state.pan),
            11 => state.expression = msb(state.expression),
            43 => state.expression = lsb(state.expression),
            64 => state.sustain = value >= 64,
            65 => {
                if value < 64 {
                    self.release_unheld_portamento_notes(channel);
                    self.channels[channel].portamento = false;
                } else {
                    state.portamento = true;
                }
            }
            99 => state.parameter_number = (value << 7) + (state.parameter_number & 0x7f),
            98 => state.parameter_number = (state.parameter_number & 0x3f80) + value,
            101 => state.parameter_number = (value << 7) + (state.parameter_number & 0x7f) + 16384,
            100 => state.parameter_number = value + (state.parameter_number & 0x3f80) + 16384,
            120 => self.all_sound_off(Some(channel)),
            121 => self.reset_controllers(Some(channel)),
            123 => self.all_notes_off(Some(channel)),
            6 => {
                if state.parameter_number == RPN_PITCH_BEND_RANGE {
                    state.pitch_bend_range = msb(state.pitch_bend_range);
                }
            }
            38 => {
                if state.parameter_number == RPN_PITCH_BEND_RANGE {
                    state.pitch_bend_range = lsb(state.pitch_bend_range);
                }
            }
            16 => state.sample_offset = msb(state.sample_offset),
            48 => state.sample_offset = lsb(state.sample_offset),
            81 => {
                if value >= 64 {
                    state.retrigger = true;
                } else {
                    self.reset_retrigger_phases(channel);
                    self.channels[channel].retrigger = false;
                }
            }
            17 => {
                let setting = msb(state.retrigger_rate_setting);
                state.set_retrigger_rate(setting);
            }
            49 => {
                let setting = lsb(state.retrigger_rate_setting);
                state.set_retrigger_rate(setting);
            }
            _ => {}
        }
    }

    /// Starts a note.
    fn note_on(&mut self, channel: usize, key: i32, velocity: i32) {
        self.note_off(channel, key);

        if self.channels[channel].portamento {
            // Glide the most recent held note on this channel to the new key instead of
            // starting a new one.
            for note_ref in self.notes.iter().rev() {
                let mut note = note_ref.borrow_mut();
                if note.channel == channel && note.is_held() {
                    self.held_notes[channel][note.key as usize] = None;
                    self.held_notes[channel][key as usize] = Some(note_ref.clone());
                    let current_pitch =
                        note.pitch + ((note.portamento_progress * note.portamento_offset) >> 12);
                    note.portamento_progress = PORTAMENTO_START;
                    note.pitch += (key - note.key) << 8;
                    note.portamento_offset = current_pitch - note.pitch;
                    note.key = key;
                    return;
                }
            }
        }

        let Some(patch) = self.patches.get(&self.channels[channel].patch).cloned() else {
            return;
        };
        let k = key as usize;
        let Some(sample) = patch.key_samples[k].clone() else {
            return;
        };
        let mut note = ActiveNote {
            channel,
            key,
            articulation: patch.key_articulation[k].clone(),
            exclusive_class: patch.key_exclusive_class[k] as i32,
            volume: (patch.key_volume[k] as i32)
                .wrapping_mul(velocity)
                .wrapping_mul(velocity)
                .wrapping_mul(patch.volume)
                .wrapping_add(1024)
                >> 11,
            pan: patch.key_pan[k] as i32,
            pitch: (key << 8) - (patch.key_tuning[k] as i32 & 0x7fff),
            release_position: -1,
            attack_position: 0,
            attack_index: 0,
            release_index: 0,
            decay_position: 0,
            portamento_offset: 0,
            portamento_progress: 0,
            vibrato_phase: 0,
            ticks: 0,
            samples_until_tick: 0,
            retrigger_phase: 0,
            sample: Some(sample.clone()),
            patch: Some(patch.clone()),
            voice: None,
        };

        let looped = patch.key_loops(k);
        let step = self.note_step(&note);
        let pan = self.note_pan(&note);
        if self.channels[channel].sample_offset == 0 {
            let volume = self.note_volume(&note);
            note.voice = SampleVoice::new(sample, step, volume, pan);
        } else {
            note.voice = SampleVoice::new(sample, step, 0, pan);
            self.apply_sample_offset(&mut note, looped);
        }
        if looped {
            if let Some(voice) = &mut note.voice {
                voice.set_loops(-1);
            }
        }

        let class = note.exclusive_class;
        let note_ref = Rc::new(RefCell::new(note));
        if class >= 0 {
            if let Some(previous) = &self.exclusive_notes[channel][class as usize] {
                let mut previous = previous.borrow_mut();
                if previous.is_held() {
                    self.held_notes[channel][previous.key as usize] = None;
                    previous.release_position = 0;
                }
            }
            self.exclusive_notes[channel][class as usize] = Some(note_ref.clone());
        }
        self.notes.push(note_ref.clone());
        self.held_notes[channel][k] = Some(note_ref);
    }

    /// Releases the note held on a key. With portamento on, the last held note on the channel
    /// keeps sounding (so a following note can glide from it).
    fn note_off(&mut self, channel: usize, key: i32) {
        let Some(note_ref) = self.held_notes[channel][key as usize].take() else {
            return;
        };
        if !self.channels[channel].portamento {
            note_ref.borrow_mut().release_position = 0;
            return;
        }
        let note_channel = note_ref.borrow().channel;
        let another_held = self.notes.iter().any(|other| {
            !Rc::ptr_eq(other, &note_ref) && {
                let other = other.borrow();
                other.channel == note_channel && other.is_held()
            }
        });
        if another_held {
            note_ref.borrow_mut().release_position = 0;
        }
    }

    /// Stops notes immediately with a one-tick fade (CC 120, or every channel when `None`).
    fn all_sound_off(&mut self, channel: Option<usize>) {
        let fade = self.tick_samples();
        let notes = std::mem::take(&mut self.notes);
        for note_ref in notes {
            let mut note = note_ref.borrow_mut();
            if channel.is_some_and(|channel| note.channel != channel) {
                drop(note);
                self.notes.push(note_ref);
                continue;
            }
            if let Some(mut voice) = note.voice.take() {
                voice.fade_out(fade);
                if voice.is_ramping() {
                    self.add_fading_voice(voice);
                }
                note.release_resources();
            }
            if note.is_held() {
                self.held_notes[note.channel][note.key as usize] = None;
            }
        }
    }

    /// Releases all held notes (CC 123, or every channel when `None`).
    fn all_notes_off(&mut self, channel: Option<usize>) {
        for note_ref in &self.notes {
            let mut note = note_ref.borrow_mut();
            if channel.is_none_or(|channel| note.channel == channel) && note.is_held() {
                self.held_notes[note.channel][note.key as usize] = None;
                note.release_position = 0;
            }
        }
    }

    /// Resets controllers to their defaults (CC 121, or every channel when `None`).
    fn reset_controllers(&mut self, channel: Option<usize>) {
        let Some(channel) = channel else {
            for channel in 0..CHANNEL_COUNT {
                self.reset_controllers(Some(channel));
            }
            return;
        };
        // Turning portamento and retrigger off has side effects on the channel's notes.
        self.release_unheld_portamento_notes(channel);
        self.reset_retrigger_phases(channel);
        self.channels[channel].reset_controllers_values();
    }

    /// When portamento is switched off, notes kept sounding for a glide but no longer held
    /// by a key are released.
    fn release_unheld_portamento_notes(&mut self, channel: usize) {
        if !self.channels[channel].portamento {
            return;
        }
        for note_ref in &self.notes {
            let mut note = note_ref.borrow_mut();
            if note.channel == channel
                && self.held_notes[channel][note.key as usize].is_none()
                && note.is_held()
            {
                note.release_position = 0;
            }
        }
    }

    /// When retrigger is switched off, resets the retrigger phase of the channel's notes.
    fn reset_retrigger_phases(&mut self, channel: usize) {
        if !self.channels[channel].retrigger {
            return;
        }
        for note_ref in &self.notes {
            let mut note = note_ref.borrow_mut();
            if note.channel == channel {
                note.retrigger_phase = 0;
            }
        }
    }

    /// Moves a voice to the fading list.
    fn add_fading_voice(&mut self, mut voice: SampleVoice) {
        voice.unlink_requested = false;
        self.fading_voices.push(voice);
    }

    /// Removes a finished note from the note list and its exclusive class slot.
    fn remove_note(&mut self, note_ref: &NoteRef, channel: usize, exclusive_class: i32) {
        self.notes.retain(|other| !Rc::ptr_eq(other, note_ref));
        // The original client only clears classes above 0 here, so class 0 keeps a stale reference.
        if exclusive_class > 0 {
            let slot = &mut self.exclusive_notes[channel][exclusive_class as usize];
            if slot
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, note_ref))
            {
                *slot = None;
            }
        }
    }

    /// Whether `note_ref` is the most recent note of its exclusive class.
    fn is_exclusive_owner(&self, note_ref: &NoteRef, channel: usize, exclusive_class: i32) -> bool {
        exclusive_class >= 0
            && self.exclusive_notes[channel][exclusive_class as usize]
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, note_ref))
    }

    /// Mixes the fading voices, then every note, for `count` frames from `offset`, running
    /// control ticks as they fall due.
    fn render_notes(&mut self, out: &mut [i32], offset: i32, count: i32) {
        self.mix_fading_voices(out, offset, count);

        let notes = self.notes.clone();
        for note_ref in &notes {
            if self.discard_if_silent(note_ref) {
                continue;
            }
            let mut remaining = count;
            let mut position = offset;
            loop {
                let until_tick = note_ref.borrow().samples_until_tick;
                if remaining <= until_tick {
                    self.render_note(note_ref, out, position, remaining, position + remaining);
                    note_ref.borrow_mut().samples_until_tick -= remaining;
                    break;
                }
                self.render_note(note_ref, out, position, until_tick, remaining + position);
                remaining -= until_tick;
                position += until_tick;
                if self.control_tick(note_ref, remaining, position, out) {
                    break;
                }
            }
        }
    }

    /// Mixes the voices fading out after their notes ended. Voices the output stage marked
    /// inactive are advanced silently.
    fn mix_fading_voices(&mut self, out: &mut [i32], offset: i32, count: i32) {
        for voice in self.fading_voices.iter_mut() {
            if voice.active {
                voice.read(out, offset as usize, count as usize, self.format);
            } else {
                voice.skip(count);
            }
        }
        self.fading_voices.retain(|voice| !voice.unlink_requested);
    }

    /// Handles notes without a voice: released ones are removed, held ones are skipped until
    /// they are released. Returns true if the note should not be rendered.
    fn discard_if_silent(&mut self, note_ref: &NoteRef) -> bool {
        let note = note_ref.borrow();
        if note.voice.is_some() {
            return false;
        }
        if !note.is_held() {
            let (channel, class) = (note.channel, note.exclusive_class);
            drop(note);
            self.remove_note(note_ref, channel, class);
        }
        true
    }

    /// Renders `count` frames of a note's voice from `offset`, handling retriggering.
    /// `out_end` is the end of the whole render request; a retriggered voice's fade-out is
    /// rendered up to there before it moves to the fading list.
    fn render_note(
        &mut self,
        note_ref: &NoteRef,
        out: &mut [i32],
        mut offset: i32,
        mut count: i32,
        out_end: i32,
    ) {
        let mut note = note_ref.borrow_mut();
        let channel = note.channel;
        if self.channels[channel].retrigger && note.is_held() {
            let rate = self.channels[channel].retrigger_rate / self.sample_rate;
            loop {
                let until_retrigger =
                    (rate + RETRIGGER_PHASE_ONE - 1 - note.retrigger_phase) / rate;
                if count < until_retrigger {
                    note.retrigger_phase += count * rate;
                    break;
                }
                count -= until_retrigger;
                let mut old_voice = note.voice.take().expect("retriggered note has a voice");
                old_voice.read(out, offset as usize, until_retrigger as usize, self.format);

                let fade = self.tick_samples().min(262144 / rate);
                note.retrigger_phase += rate * until_retrigger - RETRIGGER_PHASE_ONE;

                // Restart the sample with a new voice; the old one fades out.
                let sample = note.sample.clone().expect("retriggered note has a sample");
                let looped = note
                    .patch
                    .as_ref()
                    .expect("retriggered note has a patch")
                    .key_loops(note.key as usize);
                if self.channels[channel].sample_offset == 0 {
                    note.voice = SampleVoice::new(
                        sample,
                        old_voice.rate(),
                        old_voice.target_volume(),
                        old_voice.target_pan(),
                    );
                } else {
                    note.voice =
                        SampleVoice::new(sample, old_voice.rate(), 0, old_voice.target_pan());
                    self.apply_sample_offset(&mut note, looped);
                    if let Some(voice) = &mut note.voice {
                        voice.ramp_volume(fade, old_voice.target_volume());
                    }
                }
                if looped {
                    if let Some(voice) = &mut note.voice {
                        voice.set_loops(-1);
                    }
                }

                offset += until_retrigger;
                old_voice.fade_out(fade);
                old_voice.read(
                    out,
                    offset as usize,
                    (out_end - offset) as usize,
                    self.format,
                );
                if old_voice.is_ramping() {
                    self.add_fading_voice(old_voice);
                }
            }
        }
        if let Some(voice) = &mut note.voice {
            voice.read(out, offset as usize, count as usize, self.format);
        }
    }

    /// Runs a control tick for a note: updates pitch and envelopes and starts the ramp to the
    /// new volume and pan. If the note has finished, its voice fades out over `remaining`
    /// frames of the current request (mixed into `out` from `offset`) and the note is
    /// removed. Returns true if the note finished.
    fn control_tick(
        &mut self,
        note_ref: &NoteRef,
        remaining: i32,
        offset: i32,
        out: &mut [i32],
    ) -> bool {
        let mut note = note_ref.borrow_mut();
        let channel = note.channel;
        let class = note.exclusive_class;
        note.samples_until_tick = self.tick_samples();

        if !note.is_held() && note.voice.as_ref().is_none_or(|voice| voice.is_finished()) {
            note.release_resources();
            drop(note);
            self.remove_note(note_ref, channel, class);
            return true;
        }

        // Portamento glide.
        let mut progress = note.portamento_progress;
        if progress > 0 {
            progress -= (2.0f64
                .powf(self.channels[channel].portamento_time as f64 * 4.921259842519685e-4)
                * 16.0
                + 0.5) as i32;
            if progress < 0 {
                progress = 0;
            }
            note.portamento_progress = progress;
        }
        let step = self.note_step(&note);
        if let Some(voice) = &mut note.voice {
            voice.set_rate(step);
        }

        let articulation = note
            .articulation
            .clone()
            .expect("sounding note has an articulation");
        note.vibrato_phase += articulation.vibrato_rate;
        note.ticks += 1;

        // Envelope rates scale with the key's distance from middle C (including any glide): the
        // rate is multiplied by 2^(scaling * semitones / 768), so a scaling of 64 doubles the
        // rate per octave.
        let key_scale = (((note.key - 60) << 8)
            + ((note.portamento_progress * note.portamento_offset) >> 12))
            as f64
            * 5.086263020833333e-6;
        let mut finished = false;

        if articulation.decay_rate > 0 {
            note.decay_position += if articulation.decay_key_scaling > 0 {
                (2.0f64.powf(key_scale * articulation.decay_key_scaling as f64) * 128.0 + 0.5)
                    as i32
            } else {
                ENVELOPE_STEP
            };
            // Old School does not have this cutoff: decaying notes fade on until released.
            if self.engine.decay_ends_notes()
                && note.decay_position.wrapping_mul(articulation.decay_rate) >= 819200
            {
                finished = true;
            }
        }

        if let Some(envelope) = &articulation.attack_envelope {
            note.attack_position += if articulation.attack_key_scaling <= 0 {
                ENVELOPE_STEP
            } else {
                (2.0f64.powf(articulation.attack_key_scaling as f64 * key_scale) * 128.0 + 0.5)
                    as i32
            };
            let last = envelope.len() - 1;
            while note.attack_index < last
                && note.attack_position > (envelope[note.attack_index + 1].time as i32) << 8
            {
                note.attack_index += 1;
            }
            if note.attack_index == last && envelope[last].level == 0 {
                finished = true;
            }
        }

        if !note.is_held()
            && !self.channels[channel].sustain
            && !self.is_exclusive_owner(note_ref, channel, class)
        {
            if let Some(envelope) = &articulation.release_envelope {
                note.release_position += if articulation.release_key_scaling > 0 {
                    (2.0f64.powf(articulation.release_key_scaling as f64 * key_scale) * 128.0 + 0.5)
                        as i32
                } else {
                    ENVELOPE_STEP
                };
                let last = envelope.len() - 1;
                while last > note.release_index
                    && (envelope[note.release_index + 1].time as i32) << 8 < note.release_position
                {
                    note.release_index += 1;
                }
                if note.release_index == last {
                    finished = true;
                }
            }
        }

        if !finished {
            let volume = self.note_volume(&note);
            let pan = self.note_pan(&note);
            let samples = note.samples_until_tick;
            if let Some(voice) = &mut note.voice {
                voice.ramp_to(samples, volume, pan);
            }
            return false;
        }

        let samples = note.samples_until_tick;
        if let Some(mut voice) = note.voice.take() {
            voice.fade_out(samples);
            voice.read(out, offset as usize, remaining as usize, self.format);
            if voice.is_ramping() {
                self.add_fading_voice(voice);
            }
        }
        note.release_resources();
        if !note.is_held() {
            drop(note);
            self.remove_note(note_ref, channel, class);
        }
        true
    }

    /// Starts a note's voice part-way into its sample according to the channel's sample
    /// offset controller. For ping-pong looped samples the offset spans the sample plus one
    /// backwards pass of the loop.
    fn apply_sample_offset(&self, note: &mut ActiveNote, looped: bool) {
        let offset = self.channels[note.channel].sample_offset as i64;
        let sample = note.sample.as_ref().expect("note has a sample");
        let length = sample.len();
        let Some(voice) = note.voice.as_mut() else {
            return;
        };
        let position = if looped && sample.ping_pong {
            let total = length + length - sample.loop_start;
            let length_fixed = length << 8;
            let mut position = ((total as i64 * offset) >> 6) as i32;
            if position >= length_fixed {
                voice.play_backwards();
                position = length_fixed + length_fixed - position - 1;
            }
            position
        } else {
            ((length as i64 * offset) >> 6) as i32
        };
        voice.set_position(position);
    }

    /// Computes a note's playback step (24.8 fixed point) from its pitch, pitch bend,
    /// portamento and vibrato.
    fn note_step(&self, note: &ActiveNote) -> i32 {
        let channel = &self.channels[note.channel];
        let articulation = note
            .articulation
            .as_ref()
            .expect("note has an articulation");
        let sample = note.sample.as_ref().expect("note has a sample");

        let mut pitch = ((note.portamento_offset * note.portamento_progress) >> 12) + note.pitch;
        pitch += (channel.pitch_bend_range * (channel.pitch_bend - CONTROLLER_CENTER)) >> 12;
        if articulation.vibrato_rate > 0
            && (articulation.vibrato_depth > 0 || channel.modulation > 0)
        {
            let mut depth = articulation.vibrato_depth << 2;
            let delay = articulation.vibrato_delay << 1;
            if delay > note.ticks {
                depth = note.ticks * depth / delay;
            }
            depth += channel.modulation >> 7;
            // 0.0122718... = 2 pi / 512
            let wave = ((note.vibrato_phase & 0x1ff) as f64 * 0.01227184630308513).sin();
            pitch += (depth as f64 * wave) as i32;
        }

        // 3.2552e-4 = 1 / (12 * 256): pitch is in 1/256 semitones.
        let step = ((sample.sample_rate * 256) as f64
            * 2.0f64.powf(pitch as f64 * 3.255208333333333e-4)
            / self.sample_rate as f64
            + 0.5) as i32;
        step.max(1)
    }

    /// Computes a note's pan (0..=16384) from the note's pan and the channel pan.
    fn note_pan(&self, note: &ActiveNote) -> i32 {
        let channel_pan = self.channels[note.channel].pan;
        if channel_pan < PAN_CENTER {
            (note.pan * channel_pan + 32) >> 6
        } else {
            16384 - (((128 - note.pan) * (16384 - channel_pan) + 32) >> 6)
        }
    }

    /// Computes a note's volume from channel volume and expression, the note's velocity
    /// volume, the master volume, decay and the attack/release envelopes.
    fn note_volume(&self, note: &ActiveNote) -> i32 {
        let channel = &self.channels[note.channel];
        if channel.gain == 0 {
            return 0;
        }
        let articulation = note
            .articulation
            .as_ref()
            .expect("note has an articulation");

        // Channel volume and expression combine with a square law.
        let mut level = (channel.volume.wrapping_mul(channel.expression) + 4096) >> 13;
        level = (level.wrapping_mul(level) + 16384) >> 15;
        level = (note.volume.wrapping_mul(level) + 16384) >> 15;
        level = (level.wrapping_mul(self.master_volume) + 128) >> 8;
        let mut volume = (channel.gain.wrapping_mul(level) + 128) >> 8;

        if articulation.decay_rate > 0 {
            volume = (0.5f64
                .powf(note.decay_position as f64 * 1.953125e-5 * articulation.decay_rate as f64)
                * volume as f64
                + 0.5) as i32;
        }
        if let Some(envelope) = &articulation.attack_envelope {
            let gain = envelope_gain(envelope, note.attack_index, note.attack_position);
            volume = (gain.wrapping_mul(volume) + 32) >> 6;
        }
        if note.release_position > 0 {
            if let Some(envelope) = &articulation.release_envelope {
                let gain = envelope_gain(envelope, note.release_index, note.release_position);
                volume = (volume.wrapping_mul(gain) + 32) >> 6;
            }
        }
        volume
    }

    /// Returns each note voice's priority, sample identity and audibility, in note order, for
    /// the output stage's voice limiting.
    pub fn voice_candidates(&self) -> Vec<VoiceCandidate> {
        self.notes
            .iter()
            .filter_map(|note_ref| {
                let note = note_ref.borrow();
                note.voice.as_ref().map(|voice| VoiceCandidate {
                    priority: voice.priority(),
                    sample: Rc::as_ptr(&voice.sample),
                    audible: voice.is_audible(),
                })
            })
            .collect()
    }

    /// Sets which note voices may render when they later fade out, in the order returned by
    /// [`MidiSynth::voice_candidates`].
    pub fn set_voice_activity(&mut self, active: &[bool]) {
        let mut flags = active.iter();
        for note_ref in &self.notes {
            let mut note = note_ref.borrow_mut();
            if let Some(voice) = &mut note.voice {
                voice.active = *flags.next().expect("activity flag for every voice");
            }
        }
    }
}

/// Information the output stage needs to rank a voice.
#[derive(Debug, Clone, Copy)]
pub struct VoiceCandidate {
    /// Loudness estimate, 0..=255.
    pub priority: i32,
    /// Identity of the sample the voice plays.
    pub sample: *const Sample,
    /// Whether the voice currently produces sound.
    pub audible: bool,
}

/// Interpolates an articulation envelope's gain (64 = unity) at `position`, within the
/// segment starting at point `index`.
fn envelope_gain(envelope: &[super::patch::EnvelopePoint], index: usize, position: i32) -> i32 {
    let mut gain = envelope[index].level as i32;
    if envelope.len() - 1 > index {
        let start = (envelope[index].time as i32) << 8;
        let end = (envelope[index + 1].time as i32) << 8;
        gain += (envelope[index + 1].level as i32 - gain) * (position - start) / (end - start);
    }
    gain
}
