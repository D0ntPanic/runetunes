//! Instrument patches: the mapping from MIDI keys to samples and playback parameters.

use super::sample::Sample;
use super::sample_bank::{SampleBank, SampleSource};
use crate::buffer::Buffer;
use anyhow::{anyhow, Result};
use std::rc::Rc;

/// Number of MIDI keys.
pub const KEY_COUNT: usize = 128;

/// One point of an articulation envelope.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvelopePoint {
    /// Time of the point in units of 20 ms. (A note's envelope position advances by 128 per
    /// 10 ms control tick, scaled by key scaling, and is compared against `time << 8`.)
    pub time: u8,
    /// Gain at this point, where 64 is unity.
    pub level: i8,
}

/// Envelopes and modulation settings shared by a range of keys in a patch.
///
/// All rates are applied once per control tick (every 10 ms of output), and "key scaling"
/// parameters make the rate depend on the distance of the played key from middle C.
#[derive(Debug, Clone, Default)]
pub struct Articulation {
    /// Gain envelope applied from note-on. Reaching a final point with level 0 ends the note.
    pub attack_envelope: Option<Vec<EnvelopePoint>>,
    /// Gain envelope applied after note-off. It starts at unity and its final point always has
    /// level 0; reaching it ends the note.
    pub release_envelope: Option<Vec<EnvelopePoint>>,
    /// Exponential decay speed while the note sounds: the volume halves every `4 / decay_rate`
    /// seconds (before key scaling), and the note ends once it has decayed by 96 dB. 0 disables
    /// decay.
    pub decay_rate: i32,
    /// Key scaling of the decay speed: the speed is multiplied by `2^(scaling * semitones / 768)`
    /// for keys above middle C (and divided below it).
    pub decay_key_scaling: i32,
    /// Key scaling of the attack envelope speed, as for `decay_key_scaling`.
    pub attack_key_scaling: i32,
    /// Key scaling of the release envelope speed, as for `decay_key_scaling`.
    pub release_key_scaling: i32,
    /// Vibrato phase increment per 10 ms control tick, where 512 is one cycle (so the frequency
    /// is `rate * 100 / 512` Hz). 0 disables vibrato.
    pub vibrato_rate: i32,
    /// Vibrato depth (peak pitch deviation) in 1/64 semitones.
    pub vibrato_depth: i32,
    /// Time over which the vibrato depth fades in after note-on, in units of 20 ms.
    pub vibrato_delay: i32,
}

/// An instrument patch from cache archive 15.
///
/// A patch is indexed by the MIDI program number (including bank select bits) and maps each of
/// the 128 keys to a sample with its own tuning, volume, pan, articulation and exclusive class.
/// Adjacent keys usually share settings, so the file stores most per-key tables as run lengths.
#[derive(Debug, Clone)]
pub struct Patch {
    /// Overall patch volume (1..=256).
    pub volume: i32,
    /// Per-key volume. Stored as a signed byte like the client, so values above 127 wrap.
    pub key_volume: [i8; KEY_COUNT],
    /// Per-key pan, 0 (left) to 128 (right), 64 is centre.
    pub key_pan: [u8; KEY_COUNT],
    /// Per-key tuning. The low 15 bits are the sample's root pitch in 1/256 semitones (i.e. the
    /// key number at which the sample plays at its native rate, times 256); the sign bit marks
    /// a looping sample.
    pub key_tuning: [i16; KEY_COUNT],
    /// Per-key exclusive class (or -1). Starting a note releases any other held note in the
    /// same channel and class, as used for hi-hat choke groups.
    pub key_exclusive_class: [i8; KEY_COUNT],
    /// Per-key articulation.
    pub key_articulation: [Option<Rc<Articulation>>; KEY_COUNT],
    /// Per-key sample, once loaded.
    pub key_samples: [Option<Rc<Sample>>; KEY_COUNT],
    /// Per-key sample reference (0 = none). After subtracting one, bit 0 selects the source
    /// (0 = synth archive, 1 = Vorbis archive), bit 1 marks looping (mirrored into
    /// `key_tuning`) and the remaining bits are the sample ID. Cleared once the key's sample
    /// is loaded.
    key_sample_refs: [i32; KEY_COUNT],
}

/// Reads a zero-terminated table of signed run lengths.
fn read_run_lengths(data: &mut Buffer) -> Result<Vec<i8>> {
    let mut runs = Vec::new();
    loop {
        let value = data.get_i8()?;
        if value == 0 {
            return Ok(runs);
        }
        runs.push(value);
    }
}

/// Iterates over run lengths: yields `true` whenever a new run starts. Once the table is
/// exhausted the final run continues forever.
struct RunCursor<'a> {
    runs: &'a [i8],
    next_run: usize,
    remaining: i32,
}

impl<'a> RunCursor<'a> {
    fn new(runs: &'a [i8]) -> Self {
        RunCursor {
            runs,
            next_run: 0,
            remaining: 0,
        }
    }

    /// Advances by one key and returns whether that key starts a new run.
    fn advance(&mut self) -> bool {
        let starts_run = self.remaining == 0;
        if starts_run {
            self.remaining = match self.runs.get(self.next_run) {
                Some(&run) => {
                    self.next_run += 1;
                    run as i32
                }
                None => -1,
            };
        }
        self.remaining -= 1;
        starts_run
    }
}

/// Rounding division used when interpolating key curves: floor division for positive divisors.
fn floor_div(value: i32, divisor: i32) -> i32 {
    let sign = ((value as u32) >> 31) as i32;
    (value + sign) / divisor - sign
}

/// Reads an envelope's point times (cumulative deltas; the first point stays at time 0).
fn read_envelope_times(data: &mut Buffer, envelope: &mut [EnvelopePoint]) -> Result<()> {
    let mut time = 0i32;
    for point in envelope[1..].iter_mut() {
        time += data.get_u8()? as i32 + 1;
        point.time = time as u8;
    }
    Ok(())
}

/// Reads a key curve's X coordinates (cumulative).
fn read_curve_positions(data: &mut Buffer, curve: &mut [i8]) -> Result<()> {
    let mut position = data.get_u8()? as i32;
    curve[0] = position as i8;
    for i in (2..curve.len()).step_by(2) {
        position += data.get_u8()? as i32 + 1;
        curve[i] = position as i8;
    }
    Ok(())
}

impl Patch {
    /// Decodes a patch file.
    pub fn decode(data: Vec<u8>) -> Result<Self> {
        let mut data = Buffer::new(data);
        let mut patch = Patch {
            volume: 0,
            key_volume: [0; KEY_COUNT],
            key_pan: [0; KEY_COUNT],
            key_tuning: [0; KEY_COUNT],
            key_exclusive_class: [0; KEY_COUNT],
            key_articulation: std::array::from_fn(|_| None),
            key_samples: std::array::from_fn(|_| None),
            key_sample_refs: [0; KEY_COUNT],
        };

        // Run-length tables are each followed by one value per run (plus one for the final,
        // unbounded run). The exclusive class and pan values are read later by offset.
        let class_runs = read_run_lengths(&mut data)?;
        let class_values_offset = data.offset();
        data.seek_forward(class_runs.len() + 1)?;

        let pan_runs = read_run_lengths(&mut data)?;
        let pan_values_offset = data.offset();
        data.seek_forward(pan_runs.len() + 1)?;

        // Articulation runs. Each run refers to an articulation by index: run 0 uses
        // articulation 0, run 1 uses articulation 1, and later runs either introduce a new
        // articulation (0) or refer back to an earlier one.
        let articulation_runs = read_run_lengths(&mut data)?;
        let reference_count = articulation_runs.len() + 1;
        let mut run_articulations = vec![0usize; reference_count];
        let articulation_count = if reference_count <= 1 {
            reference_count
        } else {
            let mut count = 2;
            run_articulations[1] = 1;
            let mut previous = 1usize;
            for reference in run_articulations.iter_mut().skip(2) {
                let mut value = data.get_u8()? as usize;
                if value == 0 {
                    previous = count;
                    count += 1;
                } else {
                    // Indices at or below the previous one are offset by one so that the
                    // previous articulation itself never needs to be encoded.
                    if value <= previous {
                        value -= 1;
                    }
                    previous = value;
                }
                *reference = previous;
            }
            count
        };

        let mut articulations = vec![Articulation::default(); articulation_count];
        for articulation in articulations.iter_mut() {
            let attack_points = data.get_u8()? as usize;
            if attack_points > 0 {
                articulation.attack_envelope = Some(vec![EnvelopePoint::default(); attack_points]);
            }
            let release_points = data.get_u8()? as usize;
            if release_points > 0 {
                let mut envelope = vec![EnvelopePoint::default(); release_points + 1];
                envelope[0].level = 64;
                articulation.release_envelope = Some(envelope);
            }
        }

        // Optional key curves scaling volume and pan across the keyboard, stored as
        // (key, value) pairs.
        let volume_curve_points = data.get_u8()? as usize;
        let mut volume_curve = vec![0i8; volume_curve_points * 2];
        let pan_curve_points = data.get_u8()? as usize;
        let mut pan_curve = vec![0i8; pan_curve_points * 2];

        let sample_runs = read_run_lengths(&mut data)?;

        // Root pitch per key, as delta-coded low then high bytes.
        let mut accumulator = 0i32;
        for tuning in patch.key_tuning.iter_mut() {
            accumulator += data.get_u8()? as i32;
            *tuning = accumulator as i16;
        }
        accumulator = 0;
        for tuning in patch.key_tuning.iter_mut() {
            accumulator += data.get_u8()? as i32;
            *tuning = (*tuning as i32 + (accumulator << 8)) as i16;
        }

        // Sample reference per run of keys. Bit 1 of (reference - 1) marks a looping sample,
        // which is mirrored into the sign bit of the key's tuning.
        let mut runs = RunCursor::new(&sample_runs);
        let mut reference = 0i32;
        for key in 0..KEY_COUNT {
            if runs.advance() {
                reference = data.get_var_int()? as i32;
            }
            patch.key_tuning[key] =
                (patch.key_tuning[key] as i32 + (((reference - 1) & 2) << 14)) as i16;
            patch.key_sample_refs[key] = reference;
        }

        // Exclusive class, pan and articulation runs only count keys that have a sample.
        let mut runs = RunCursor::new(&class_runs);
        let mut value_offset = class_values_offset;
        let mut class = 0i8;
        for key in 0..KEY_COUNT {
            if patch.key_sample_refs[key] != 0 {
                if runs.advance() {
                    class = (data.get_at(value_offset, 1)?[0] as i8).wrapping_sub(1);
                    value_offset += 1;
                }
                patch.key_exclusive_class[key] = class;
            }
        }

        let mut runs = RunCursor::new(&pan_runs);
        let mut value_offset = pan_values_offset;
        let mut pan = 0i32;
        for key in 0..KEY_COUNT {
            if patch.key_sample_refs[key] != 0 {
                if runs.advance() {
                    pan = ((data.get_at(value_offset, 1)?[0] as i8) as i32 + 16) << 2;
                    value_offset += 1;
                }
                patch.key_pan[key] = pan as u8;
            }
        }

        // Articulation per key, by run.
        let mut runs = RunCursor::new(&articulation_runs);
        let mut reference_index = 0;
        let mut current = 0;
        let mut key_articulation_index = [None; KEY_COUNT];
        for key in 0..KEY_COUNT {
            if patch.key_sample_refs[key] != 0 {
                if runs.remaining == 0 {
                    current = run_articulations[reference_index];
                    if reference_index < articulation_runs.len() {
                        reference_index += 1;
                    }
                }
                runs.advance();
                key_articulation_index[key] = Some(current);
            }
        }

        // Per-key volume. These runs advance on every key (not just keys with samples) and
        // share the sample run table.
        let mut runs = RunCursor::new(&sample_runs);
        let mut volume = 0i32;
        for key in 0..KEY_COUNT {
            if runs.advance() && patch.key_sample_refs[key] > 0 {
                volume = data.get_u8()? as i32 + 1;
            }
            patch.key_volume[key] = volume as i8;
        }
        patch.volume = data.get_u8()? as i32 + 1;

        // Envelope levels.
        for articulation in articulations.iter_mut() {
            if let Some(envelope) = &mut articulation.attack_envelope {
                for point in envelope.iter_mut() {
                    point.level = data.get_i8()?;
                }
            }
            if let Some(envelope) = &mut articulation.release_envelope {
                // The first point is fixed at unity and the last at silence.
                let last = envelope.len() - 1;
                for point in envelope[1..last].iter_mut() {
                    point.level = data.get_i8()?;
                }
            }
        }
        for i in (1..volume_curve.len()).step_by(2) {
            volume_curve[i] = data.get_i8()?;
        }
        for i in (1..pan_curve.len()).step_by(2) {
            pan_curve[i] = data.get_i8()?;
        }

        // Envelope times (cumulative). The first point of each envelope is at time 0.
        for articulation in articulations.iter_mut() {
            if let Some(envelope) = &mut articulation.release_envelope {
                read_envelope_times(&mut data, envelope)?;
            }
        }
        for articulation in articulations.iter_mut() {
            if let Some(envelope) = &mut articulation.attack_envelope {
                read_envelope_times(&mut data, envelope)?;
            }
        }

        if !volume_curve.is_empty() {
            read_curve_positions(&mut data, &mut volume_curve)?;
            patch.apply_volume_curve(&volume_curve)?;
        }
        if !pan_curve.is_empty() {
            read_curve_positions(&mut data, &mut pan_curve)?;
            patch.apply_pan_curve(&pan_curve)?;
        }

        for articulation in articulations.iter_mut() {
            articulation.decay_rate = data.get_u8()? as i32;
        }
        for articulation in articulations.iter_mut() {
            if articulation.attack_envelope.is_some() {
                articulation.attack_key_scaling = data.get_u8()? as i32;
            }
            if articulation.release_envelope.is_some() {
                articulation.release_key_scaling = data.get_u8()? as i32;
            }
            if articulation.decay_rate > 0 {
                articulation.decay_key_scaling = data.get_u8()? as i32;
            }
        }
        for articulation in articulations.iter_mut() {
            articulation.vibrato_rate = data.get_u8()? as i32;
        }
        for articulation in articulations.iter_mut() {
            if articulation.vibrato_rate > 0 {
                articulation.vibrato_depth = data.get_u8()? as i32;
            }
        }
        for articulation in articulations.iter_mut() {
            if articulation.vibrato_depth > 0 {
                articulation.vibrato_delay = data.get_u8()? as i32;
            }
        }

        let articulations: Vec<Rc<Articulation>> = articulations.into_iter().map(Rc::new).collect();
        for key in 0..KEY_COUNT {
            if let Some(index) = key_articulation_index[key] {
                patch.key_articulation[key] = Some(articulations[index].clone());
            }
        }

        Ok(patch)
    }

    /// Scales per-key volumes by a piecewise-linear curve of (key, gain) points, where a gain
    /// of 64 is unity.
    fn apply_volume_curve(&mut self, curve: &[i8]) -> Result<()> {
        let volume = &mut self.key_volume;
        let scale = |gain: i32, value: i8| ((gain * value as i32 + 32) >> 6) as i8;

        let mut x0 = curve[0] as i32;
        let mut y0 = curve[1] as i32;
        for key in 0..x0.max(0) {
            volume[checked_key(key)?] = scale(y0, volume[checked_key(key)?]);
        }
        let mut i = 2;
        while i < curve.len() {
            let x1 = curve[i] as i32;
            let y1 = curve[i + 1] as i32;
            i += 2;
            let mut accumulator = (x1 - x0) * y0 + (x1 - x0) / 2;
            for key in x0..x1 {
                let gain = floor_div(accumulator, x1 - x0);
                accumulator += y1 - y0;
                volume[checked_key(key)?] = scale(gain, volume[checked_key(key)?]);
            }
            y0 = y1;
            x0 = x1;
        }
        for key in x0..KEY_COUNT as i32 {
            volume[checked_key(key)?] = scale(y0, volume[checked_key(key)?]);
        }
        Ok(())
    }

    /// Offsets per-key pans by a piecewise-linear curve of (key, offset / 2) points, clamping
    /// the result to 0..=128.
    fn apply_pan_curve(&mut self, curve: &[i8]) -> Result<()> {
        let pan = &mut self.key_pan;
        let offset = |delta: i32, value: u8| (delta + value as i32).clamp(0, 128) as u8;

        let mut x0 = curve[0] as i32;
        let mut y0 = (curve[1] as i32) << 1;
        for key in 0..x0.max(0) {
            pan[checked_key(key)?] = offset(y0, pan[checked_key(key)?]);
        }
        let mut i = 2;
        while i < curve.len() {
            let x1 = curve[i] as i32;
            let mut accumulator = (x1 - x0) * y0 + (x1 - x0) / 2;
            let y1 = (curve[i + 1] as i32) << 1;
            i += 2;
            for key in x0..x1 {
                let delta = floor_div(accumulator, x1 - x0);
                accumulator += y1 - y0;
                pan[checked_key(key)?] = offset(delta, pan[checked_key(key)?]);
            }
            x0 = x1;
            y0 = y1;
        }
        for key in x0..KEY_COUNT as i32 {
            pan[checked_key(key)?] = offset(y0, pan[checked_key(key)?]);
        }
        Ok(())
    }

    /// Loads the samples for every key marked in `used_keys` (or every key if `None`).
    pub fn load_samples(
        &mut self,
        bank: &mut SampleBank,
        used_keys: Option<&[bool; KEY_COUNT]>,
    ) -> Result<()> {
        let mut last_reference = 0;
        let mut last_sample: Option<Rc<Sample>> = None;
        for key in 0..KEY_COUNT {
            if used_keys.is_some_and(|used| !used[key]) {
                continue;
            }
            let reference = self.key_sample_refs[key];
            if reference == 0 {
                continue;
            }
            if reference != last_reference {
                last_reference = reference;
                let code = reference - 1;
                let source = if code & 1 == 0 {
                    SampleSource::Synth
                } else {
                    SampleSource::Vorbis
                };
                last_sample = Some(bank.get(source, (code >> 2) as u32)?);
            }
            self.key_samples[key] = last_sample.clone();
            self.key_sample_refs[key] = 0;
        }
        Ok(())
    }

    /// Whether the sample for `key` loops.
    pub fn key_loops(&self, key: usize) -> bool {
        self.key_tuning[key] < 0
    }
}

/// Validates a key index computed from curve data.
fn checked_key(key: i32) -> Result<usize> {
    if (0..KEY_COUNT as i32).contains(&key) {
        Ok(key as usize)
    } else {
        Err(anyhow!("Patch key curve out of range"))
    }
}
