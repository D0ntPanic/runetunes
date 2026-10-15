//! The output stage: fixed-size chunk rendering, voice limiting and 16-bit conversion.

use super::midi_synth::MidiSynth;
use super::sample::Sample;
use super::Engine;
use std::collections::HashMap;

/// Maximum number of audible voices the voice limiter allows.
const MAX_VOICES: i32 = 32;

/// Number of priority buckets (priorities are 0..=255, bucket = priority / 32).
const BUCKET_COUNT: usize = 8;

/// Default priority of streams that don't compute one.
const DEFAULT_PRIORITY: i32 = 255;

/// Renders a [`MidiSynth`].
///
/// Every chunk ([`Engine::chunk_frames`] frames) the mix buffer is cleared and the synth mixes
/// into it. Samples are 32-bit with 8 fractional bits below the 16-bit range; [`to_i16`] clips
/// them to 16-bit output.
///
/// Every `sample_rate / 16` samples the voice limiter re-evaluates which voices are "active"
/// (see [`MusicRenderer::limit_voices`]).
pub struct MusicRenderer {
    pub synth: MidiSynth,
    /// Frames per chunk.
    chunk_frames: usize,
    /// Streams above the synth's note list (see [`Engine::stream_depth`]).
    stream_depth: usize,
    stereo: bool,
    sample_rate: i32,
    /// Samples until the voice limiter next runs.
    limiter_countdown: i32,
    /// Mix buffer for one chunk (interleaved when stereo).
    buffer: Vec<i32>,
}

impl MusicRenderer {
    pub fn new(engine: Engine, synth: MidiSynth, sample_rate: i32, stereo: bool) -> Self {
        let chunk_frames = engine.chunk_frames();
        MusicRenderer {
            synth,
            chunk_frames,
            stream_depth: engine.stream_depth(),
            stereo,
            sample_rate,
            limiter_countdown: 0,
            buffer: vec![0; chunk_frames * if stereo { 2 } else { 1 }],
        }
    }

    /// Frames rendered per chunk.
    pub fn chunk_frames(&self) -> usize {
        self.chunk_frames
    }

    /// Output sample rate in Hz.
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate
    }

    /// Number of interleaved output channels.
    pub fn channels(&self) -> usize {
        if self.stereo {
            2
        } else {
            1
        }
    }

    /// Renders the next chunk of [`Self::chunk_frames`] frames and returns the 32-bit mix
    /// buffer.
    pub fn render_chunk(&mut self) -> &[i32] {
        self.buffer.fill(0);
        self.limiter_countdown -= self.chunk_frames as i32;
        if self.limiter_countdown <= 0 {
            self.limiter_countdown += self.sample_rate >> 4;
            self.limit_voices();
        }
        if self.limiter_countdown < 0 {
            self.limiter_countdown = 0;
        }
        self.synth
            .read(&mut self.buffer, 0, self.chunk_frames as i32);
        &self.buffer
    }

    /// Chooses which voices are active, at most [`MAX_VOICES`] audible ones.
    ///
    /// The renderer walks its tree of audio streams (the streams above the note list, the
    /// note list, and each note's voice), placing each stream in one of eight buckets by
    /// priority, where a child's priority is its own loudness estimate scaled by its parent's
    /// priority. Streams other than voices have the default priority of 255, so every level of
    /// the tree above the voices scales their priority by 255/256. Buckets are
    /// visited from loudest to quietest. A stream playing a sample that already has `n` active
    /// voices is deferred until the pass that allows `n` duplicates, so repeated notes of the
    /// same sample lose out to different samples of similar loudness. Each active audible voice
    /// counts towards the limit; once it is reached the remaining voices stay inactive.
    fn limit_voices(&mut self) {
        let candidates = self.synth.voice_candidates();
        let mut active = vec![false; candidates.len()];

        // Stream tree: a chain of `stream_depth` streams (the root, then down to the synth),
        // then the note list, whose children are the note voices.
        enum Node {
            Stream(usize),
            Notes,
            Voice(usize),
        }
        let mut buckets: Vec<Vec<(Node, i32)>> = (0..BUCKET_COUNT).map(|_| Vec::new()).collect();
        let enqueue = |buckets: &mut Vec<Vec<(Node, i32)>>, node: Node, priority: i32| {
            buckets[(priority >> 5) as usize].push((node, priority));
        };
        // Active voice count per sample.
        let mut sample_counts: HashMap<*const Sample, i32> = HashMap::new();
        let sample_of = |node: &Node| match node {
            Node::Voice(index) => Some(candidates[*index].sample),
            _ => None,
        };

        enqueue(&mut buckets, Node::Stream(1), DEFAULT_PRIORITY);
        let mut voice_count = 0;
        let mut pending: u32 = (1 << BUCKET_COUNT) - 1;
        let mut level = BUCKET_COUNT as i32 - 1;
        'search: while pending != 0 {
            // Levels 7..0 visit buckets 7..0 allowing no duplicates; later passes revisit the
            // upper buckets allowing more duplicates each time.
            let (first_bucket, first_allowed) = if level < 0 {
                (level & 3, -(level >> 2))
            } else {
                (level, 0)
            };
            let mut bits = (pending >> first_bucket) & 0x1111_1111;
            let mut bucket = first_bucket as usize;
            let mut allowed_duplicates = first_allowed;
            while bits != 0 {
                if bits & 1 != 0 {
                    pending &= !(1 << bucket);
                    let mut i = 0;
                    while i < buckets[bucket].len() {
                        let sample = sample_of(&buckets[bucket][i].0);
                        let duplicates =
                            sample.map_or(0, |sample| *sample_counts.get(&sample).unwrap_or(&0));
                        if sample.is_some() && duplicates > allowed_duplicates {
                            // Too many voices of this sample for now; retry in a later pass.
                            pending |= 1 << bucket;
                            i += 1;
                            continue;
                        }

                        let (node, priority) = buckets[bucket].remove(i);
                        let audible = match node {
                            Node::Voice(index) => {
                                active[index] = true;
                                candidates[index].audible as i32
                            }
                            _ => 0,
                        };
                        voice_count += audible;
                        if let Some(sample) = sample {
                            *sample_counts.entry(sample).or_insert(0) += audible;
                        }
                        if voice_count >= MAX_VOICES {
                            break 'search;
                        }
                        let child_priority = (priority * DEFAULT_PRIORITY) >> 8;
                        match node {
                            Node::Stream(level) if level < self.stream_depth => {
                                enqueue(&mut buckets, Node::Stream(level + 1), child_priority)
                            }
                            Node::Stream(_) => enqueue(&mut buckets, Node::Notes, child_priority),
                            Node::Notes => {
                                for (index, candidate) in candidates.iter().enumerate() {
                                    enqueue(
                                        &mut buckets,
                                        Node::Voice(index),
                                        (priority * candidate.priority) >> 8,
                                    );
                                }
                            }
                            Node::Voice(_) => {}
                        }
                    }
                }
                bits >>= 4;
                bucket += 4;
                allowed_duplicates += 1;
            }
            level -= 1;
        }

        self.synth.set_voice_activity(&active);
    }
}

/// Converts a mix buffer sample to 16-bit output, saturating at the 24-bit range first.
pub fn to_i16(sample: i32) -> i16 {
    let mut sample = sample;
    if (sample.wrapping_add(0x80_0000) as u32) & 0xff00_0000 != 0 {
        sample = (sample >> 31) ^ 0x7f_ffff;
    }
    (sample >> 8) as i16
}
