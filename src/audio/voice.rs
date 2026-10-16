//! Resampling playback of a single [`Sample`].

use super::sample::Sample;
use super::sinc::{self, SincTable};
use std::rc::Rc;

/// Sentinel `target_volume` meaning "ramp to silence, then stop".
const FADE_OUT: i32 = i32::MIN;

/// Pan value for the centre (pan runs from 0 = left to 16384 = right).
pub const PAN_CENTER: i32 = 8192;

/// Plays a [`Sample`] at an arbitrary rate, mixing it into a 32-bit output buffer with
/// interpolation (see [`Interpolation`]), volume/pan ramping and forward, reverse or ping-pong
/// looping.
///
/// # Fixed-point conventions
///
/// * `position` and `step` are fixed point with `frac_bits` fractional bits (see
///   [`Engine::position_frac_bits`]). The original clients use 24.8 fixed point, so one unit
///   is 1/256 of a sample. A negative `step` plays the sample backwards.
/// * With 8-bit samples a voice adds `(sample << 8) * volume >> 6` (that is,
///   `4 * sample * volume`) to the mix, and the output stage divides the mix by 256 to produce
///   16-bit audio. A volume of 16384 therefore plays a full-scale 8-bit sample at full 16-bit
///   scale. The 16-bit ("wide") kernels add `sample * volume >> 6`, the same scale for 16-bit
///   samples; see [`MixFormat`].
/// * Volume and pan changes are applied as linear ramps over a number of samples, tracked
///   separately for the mono volume and the left/right volumes.
///
/// [`Engine::position_frac_bits`]: super::Engine::position_frac_bits
#[derive(Debug, Clone)]
pub struct SampleVoice {
    pub sample: Rc<Sample>,
    /// Loop start/end copied from the sample, in samples.
    loop_start: i32,
    loop_end: i32,
    /// Whether looping alternates direction.
    ping_pong: bool,

    /// Fractional bits of `position` and `step`.
    frac_bits: u32,
    /// Playback position, fixed point.
    position: i64,
    /// Position increment per output sample, fixed point; negative when playing backwards.
    step: i64,
    /// Loop passes remaining: 0 = don't loop, -1 = loop forever.
    loops_remaining: i32,
    /// Whether the position has wrapped or reflected at a loop boundary, so that the samples
    /// before the loop start are no longer the ones that preceded the current position.
    looped: bool,

    /// Volume the voice is heading towards, or [`FADE_OUT`].
    target_volume: i32,
    /// Pan the voice is heading towards (0..=16384, or -1 when unset).
    target_pan: i32,
    /// Current mono volume.
    volume: i32,
    /// Current left channel volume.
    left_volume: i32,
    /// Current right channel volume.
    right_volume: i32,
    /// Per-sample change of the mono volume during a ramp.
    volume_delta: i32,
    /// Per-sample change of the left volume during a ramp.
    left_delta: i32,
    /// Per-sample change of the right volume during a ramp.
    right_delta: i32,
    /// Samples remaining in the current ramp, 0 when not ramping.
    ramp_remaining: i32,

    /// Whether the output stage allows this voice to be rendered when it is mixed by the
    /// fade-out mixer (see [`MusicRenderer`](super::renderer::MusicRenderer)). Voices that are
    /// not active are advanced without producing output.
    pub active: bool,
    /// Set when the voice asks to be removed from the list that owns it: it reached the end
    /// of the sample or finished fading out.
    pub unlink_requested: bool,
}

/// Left gain for a volume and pan, using a constant-power pan law.
fn left_gain(volume: i32, pan: i32) -> i32 {
    if pan < 0 {
        volume
    } else {
        (volume as f64 * ((16384 - pan) as f64 * 1.220703125e-4).sqrt() + 0.5) as i32
    }
}

/// Right gain for a volume and pan, using a constant-power pan law.
fn right_gain(volume: i32, pan: i32) -> i32 {
    if pan < 0 {
        -volume
    } else {
        (volume as f64 * (pan as f64 * 1.220703125e-4).sqrt() + 0.5) as i32
    }
}

/// How voices reconstruct the sample between sample points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    /// Linear interpolation between neighbouring samples, as in the original clients.
    Linear,
    /// A band-limited windowed-sinc kernel (see [`sinc`](super::sinc)), which suppresses the
    /// images linear interpolation leaves when a sample plays slowly and the aliasing when it
    /// plays fast.
    Sinc,
}

/// The layout of the mix buffer and the kernels used to mix into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixFormat {
    /// The buffer holds interleaved left/right pairs.
    pub stereo: bool,
    /// Use Old School's 16-bit kernels, which interpolate as
    /// `current + ((next - current) * fraction >> 8)` and scale by `volume >> 6`. Otherwise the
    /// 8-bit kernels interpolate as `(current << 8) + (next - current) * fraction` and scale
    /// non-interpolated samples by `volume << 2`.
    pub wide: bool,
    /// Interpolation between sample points.
    pub interpolation: Interpolation,
}

impl MixFormat {
    /// Interpolates between two samples at `fraction` (0..2^`frac_bits`) of the way from
    /// `from` to `to`.
    #[inline(always)]
    fn interpolate(&self, from: i32, to: i32, fraction: i64, frac_bits: u32) -> i32 {
        let delta = (to - from) as i64 * fraction;
        if self.wide {
            from + (delta >> frac_bits) as i32
        } else {
            (from << 8) + (delta >> (frac_bits - 8)) as i32
        }
    }

    /// Shift applied to volumes in the non-interpolating kernels.
    fn volume_shift(&self) -> i32 {
        if self.wide {
            0
        } else {
            2
        }
    }

    /// Contribution of a non-interpolated sample at a volume shifted by `volume_shift`.
    #[inline(always)]
    fn scale(&self, value: i32, volume: i32) -> i32 {
        if self.wide {
            value.wrapping_mul(volume) >> 6
        } else {
            value.wrapping_mul(volume)
        }
    }
}

/// Parameters for one call of a mixing kernel.
struct Mix<'a> {
    data: &'a [i16],
    out: &'a mut [i32],
    format: MixFormat,
}

impl SampleVoice {
    /// Starts a voice at the beginning of `sample`, with `step` given in fixed point with
    /// `frac_bits` fractional bits. Returns `None` for an empty sample.
    pub fn new(
        sample: Rc<Sample>,
        frac_bits: u32,
        step: i64,
        volume: i32,
        pan: i32,
    ) -> Option<Self> {
        if sample.data.is_empty() {
            return None;
        }
        let mut voice = SampleVoice {
            loop_start: sample.loop_start,
            loop_end: sample.loop_end,
            ping_pong: sample.ping_pong,
            sample,
            frac_bits,
            position: 0,
            step,
            loops_remaining: 0,
            looped: false,
            target_volume: volume,
            target_pan: pan,
            volume: 0,
            left_volume: 0,
            right_volume: 0,
            volume_delta: 0,
            left_delta: 0,
            right_delta: 0,
            ramp_remaining: 0,
            active: true,
            unlink_requested: false,
        };
        voice.snap_to_target();
        Some(voice)
    }

    fn unlink(&mut self) {
        self.unlink_requested = true;
    }

    /// Sets the number of extra loop passes (-1 = loop forever).
    pub fn set_loops(&mut self, loops: i32) {
        self.loops_remaining = loops;
    }

    /// Fractional bits of the voice's position and step.
    pub fn frac_bits(&self) -> u32 {
        self.frac_bits
    }

    /// Current playback rate (always positive).
    pub fn rate(&self) -> i64 {
        self.step.abs()
    }

    /// Sets the playback rate, keeping the current direction.
    pub fn set_rate(&mut self, rate: i64) {
        self.step = if self.step < 0 { -rate } else { rate };
    }

    /// Makes the voice play backwards, as if it had already reflected at the end of a
    /// ping-pong loop.
    pub fn play_backwards(&mut self) {
        self.step = -self.step.abs();
        self.looped = true;
    }

    /// Length of the sample in position units.
    fn fixed_length(&self) -> i64 {
        (self.sample.len() as i64) << self.frac_bits
    }

    /// Moves the playback position (fixed point), clamped to just outside the sample.
    pub fn set_position(&mut self, position: i64) {
        self.position = position.clamp(-1, self.fixed_length());
    }

    /// Whether the position has run off either end of the sample.
    pub fn is_finished(&self) -> bool {
        self.position < 0 || self.position >= self.fixed_length()
    }

    /// Whether a volume ramp is in progress.
    pub fn is_ramping(&self) -> bool {
        self.ramp_remaining != 0
    }

    /// Whether the voice currently produces sound.
    pub fn is_audible(&self) -> bool {
        !(self.target_volume == 0 && self.ramp_remaining == 0)
    }

    /// Target volume, or 0 while fading out.
    pub fn target_volume(&self) -> i32 {
        if self.target_volume == FADE_OUT {
            0
        } else {
            self.target_volume
        }
    }

    /// Target pan, or -1 if unset.
    pub fn target_pan(&self) -> i32 {
        if self.target_pan < 0 {
            -1
        } else {
            self.target_pan
        }
    }

    /// Loudness estimate (0..=255) used by the output stage to decide which voices to keep
    /// when too many are playing. Quieter voices and voices near the end of a non-looping
    /// sample get lower priority.
    pub fn priority(&self) -> i32 {
        let mut priority = (self.volume.wrapping_mul(3) >> 6).wrapping_abs();
        if self.loops_remaining == 0 {
            // In 24.8 units, overflowing like the original client.
            let position = (self.position >> (self.frac_bits - 8)) as i32;
            priority -= (priority.wrapping_mul(position)) / (self.sample.len() << 8);
        } else if self.loops_remaining >= 0 {
            priority -= priority.wrapping_mul(self.loop_start) / self.sample.len();
        }
        priority.min(255)
    }

    /// Sets the current volumes to the targets, ending any ramp.
    fn snap_to_target(&mut self) {
        self.volume = self.target_volume;
        self.left_volume = left_gain(self.target_volume, self.target_pan);
        self.right_volume = right_gain(self.target_volume, self.target_pan);
    }

    /// Sets volume and pan immediately.
    fn set_volume_and_pan(&mut self, volume: i32, pan: i32) {
        self.target_volume = volume;
        self.target_pan = pan;
        self.ramp_remaining = 0;
        self.snap_to_target();
    }

    /// Ends any ramp by jumping straight to the target.
    fn finish_ramp(&mut self) {
        if self.ramp_remaining == 0 {
            return;
        }
        if self.target_volume == FADE_OUT {
            self.target_volume = 0;
        }
        self.ramp_remaining = 0;
        self.snap_to_target();
    }

    /// Ramps to a new volume over `samples` output samples, keeping the pan.
    pub fn ramp_volume(&mut self, samples: i32, volume: i32) {
        self.ramp_to(samples, volume, self.target_pan());
    }

    /// Ramps to a new volume and pan over (at most) `samples` output samples. The ramp is
    /// shortened so that every per-sample delta is at least one.
    pub fn ramp_to(&mut self, samples: i32, volume: i32, pan: i32) {
        if samples == 0 {
            self.set_volume_and_pan(volume, pan);
            return;
        }
        let left = left_gain(volume, pan);
        let right = right_gain(volume, pan);
        if self.left_volume == left && self.right_volume == right {
            // Note: the target volume is deliberately left unchanged here to match
            // the original client.
            self.ramp_remaining = 0;
            return;
        }
        let largest_change = (volume - self.volume)
            .max(self.volume - volume)
            .max(left - self.left_volume)
            .max(self.left_volume - left)
            .max(right - self.right_volume)
            .max(self.right_volume - right);
        let samples = samples.min(largest_change);
        self.ramp_remaining = samples;
        self.target_volume = volume;
        self.target_pan = pan;
        self.volume_delta = (volume - self.volume) / samples;
        self.left_delta = (left - self.left_volume) / samples;
        self.right_delta = (right - self.right_volume) / samples;
    }

    /// Fades to silence over (at most) `samples` output samples, after which the voice
    /// unlinks itself.
    pub fn fade_out(&mut self, samples: i32) {
        if samples == 0 {
            self.set_volume_and_pan(0, self.target_pan());
            self.unlink();
        } else if self.left_volume == 0 && self.right_volume == 0 {
            self.ramp_remaining = 0;
            self.target_volume = 0;
            self.volume = 0;
            self.unlink();
        } else {
            let largest = self
                .volume
                .wrapping_neg()
                .max(self.volume)
                .max(self.left_volume.wrapping_neg())
                .max(self.left_volume)
                .max(self.right_volume.wrapping_neg())
                .max(self.right_volume);
            let samples = samples.min(largest);
            self.ramp_remaining = samples;
            self.target_volume = FADE_OUT;
            self.volume_delta = -self.volume / samples;
            self.left_delta = -self.left_volume / samples;
            self.right_delta = -self.right_volume / samples;
        }
    }

    /// Called when a ramp's sample count runs out. Integer division can leave the volumes
    /// slightly short of their targets; in that case a new ramp with ±1 steps covers the rest.
    /// Returns true when a fade-out has completed (the voice is now silent and unlinked).
    fn on_ramp_end(&mut self) -> bool {
        let (volume, left, right) = if self.target_volume == FADE_OUT {
            (0, 0, 0)
        } else {
            (
                self.target_volume,
                left_gain(self.target_volume, self.target_pan),
                right_gain(self.target_volume, self.target_pan),
            )
        };

        if self.volume != volume || self.left_volume != left || self.right_volume != right {
            if self.volume < volume {
                self.volume_delta = 1;
                self.ramp_remaining = volume - self.volume;
            } else if self.volume > volume {
                self.volume_delta = -1;
                self.ramp_remaining = self.volume - volume;
            } else {
                self.volume_delta = 0;
            }
            if self.left_volume < left {
                self.left_delta = 1;
                if self.ramp_remaining == 0 || self.ramp_remaining > left - self.left_volume {
                    self.ramp_remaining = left - self.left_volume;
                }
            } else if self.left_volume > left {
                self.left_delta = -1;
                if self.ramp_remaining == 0 || self.ramp_remaining > self.left_volume - left {
                    self.ramp_remaining = self.left_volume - left;
                }
            } else {
                self.left_delta = 0;
            }
            if self.right_volume < right {
                self.right_delta = 1;
                if self.ramp_remaining == 0 || self.ramp_remaining > right - self.right_volume {
                    self.ramp_remaining = right - self.right_volume;
                }
            } else if self.right_volume > right {
                self.right_delta = -1;
                if self.ramp_remaining == 0 || self.ramp_remaining > self.right_volume - right {
                    self.ramp_remaining = self.right_volume - right;
                }
            } else {
                self.right_delta = 0;
            }
            false
        } else if self.target_volume == FADE_OUT {
            self.target_volume = 0;
            self.volume = 0;
            self.left_volume = 0;
            self.right_volume = 0;
            self.unlink();
            true
        } else {
            self.snap_to_target();
            false
        }
    }

    /// Mixes `count` frames into `out` starting at frame `offset`, in the given format.
    pub fn read(&mut self, out: &mut [i32], offset: usize, count: usize, format: MixFormat) {
        if self.target_volume == 0 && self.ramp_remaining == 0 {
            self.skip(count as i32);
            return;
        }
        let sample = self.sample.clone();
        let data = &sample.data[..];
        let start = (self.loop_start as i64) << self.frac_bits;
        let end = (self.loop_end as i64) << self.frac_bits;
        let length = self.fixed_length();
        let loop_length = end - start;
        if loop_length <= 0 {
            self.loops_remaining = 0;
        }

        let mut mix = Mix { data, out, format };
        let mut frame = offset as i32;
        let out_end = (count + offset) as i32;

        if self.position < 0 {
            if self.step <= 0 {
                self.finish_ramp();
                self.unlink();
                return;
            }
            self.position = 0;
        }
        if self.position >= length {
            if self.step >= 0 {
                self.finish_ramp();
                self.unlink();
                return;
            }
            self.position = length - 1;
        }

        // Samples at the loop boundaries, used to interpolate across the loop point. Make
        // sure to tolerate out-of-range loop points.
        let sample_at = |index: i32| data.get(index as usize).map_or(0, |&value| value as i32);
        let loop_first = sample_at(self.loop_start);
        let loop_last_value = sample_at(self.loop_end - 1);
        let loop_last = || loop_last_value;

        if self.loops_remaining >= 0 {
            if self.loops_remaining > 0 {
                if self.ping_pong {
                    'passes: {
                        if self.step < 0 {
                            frame = self.mix_backward(&mut mix, frame, start, out_end, loop_first);
                            if self.position >= start {
                                return;
                            }
                            self.position = start + start - self.position - 1;
                            self.step = -self.step;
                            self.looped = true;
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break 'passes;
                            }
                        }
                        loop {
                            frame = self.mix_forward(&mut mix, frame, end, out_end, loop_last());
                            if self.position < end {
                                return;
                            }
                            self.position = end + end - self.position - 1;
                            self.step = -self.step;
                            self.looped = true;
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break;
                            }
                            frame = self.mix_backward(&mut mix, frame, start, out_end, loop_first);
                            if self.position >= start {
                                return;
                            }
                            self.position = start + start - self.position - 1;
                            self.step = -self.step;
                            self.looped = true;
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break;
                            }
                        }
                    }
                } else if self.step < 0 {
                    loop {
                        frame = self.mix_backward(&mut mix, frame, start, out_end, loop_last());
                        if self.position >= start {
                            return;
                        }
                        let wraps = (end - self.position - 1) / loop_length;
                        if wraps >= self.loops_remaining as i64 {
                            self.position += loop_length * self.loops_remaining as i64;
                            self.looped = true;
                            self.loops_remaining = 0;
                            break;
                        }
                        self.position += loop_length * wraps;
                        self.looped = true;
                        self.loops_remaining -= wraps as i32;
                    }
                } else {
                    loop {
                        frame = self.mix_forward(&mut mix, frame, end, out_end, loop_first);
                        if self.position < end {
                            return;
                        }
                        let wraps = (self.position - start) / loop_length;
                        if wraps >= self.loops_remaining as i64 {
                            self.position -= loop_length * self.loops_remaining as i64;
                            self.looped = true;
                            self.loops_remaining = 0;
                            break;
                        }
                        self.position -= loop_length * wraps;
                        self.looped = true;
                        self.loops_remaining -= wraps as i32;
                    }
                }
            }

            // No loop passes left: play out to the end of the sample.
            if self.step < 0 {
                self.mix_backward(&mut mix, frame, 0, out_end, 0);
                if self.position < 0 {
                    self.position = -1;
                    self.finish_ramp();
                    self.unlink();
                }
            } else {
                self.mix_forward(&mut mix, frame, length, out_end, 0);
                if self.position >= length {
                    self.position = length;
                    self.finish_ramp();
                    self.unlink();
                }
            }
        } else if self.ping_pong {
            if self.step < 0 {
                frame = self.mix_backward(&mut mix, frame, start, out_end, loop_first);
                if self.position >= start {
                    return;
                }
                self.position = start + start - self.position - 1;
                self.step = -self.step;
                self.looped = true;
            }
            loop {
                frame = self.mix_forward(&mut mix, frame, end, out_end, loop_last());
                if self.position < end {
                    return;
                }
                self.position = end + end - self.position - 1;
                self.step = -self.step;
                self.looped = true;
                frame = self.mix_backward(&mut mix, frame, start, out_end, loop_first);
                if self.position >= start {
                    return;
                }
                self.position = start + start - self.position - 1;
                self.step = -self.step;
                self.looped = true;
            }
        } else if self.step < 0 {
            loop {
                frame = self.mix_backward(&mut mix, frame, start, out_end, loop_last());
                if self.position >= start {
                    return;
                }
                self.position = end - (end - 1 - self.position) % loop_length - 1;
                self.looped = true;
            }
        } else {
            loop {
                frame = self.mix_forward(&mut mix, frame, end, out_end, loop_first);
                if self.position < end {
                    return;
                }
                self.position = start + (self.position - start) % loop_length;
                self.looped = true;
            }
        }
    }

    /// Advances the voice by `count` frames without producing output.
    pub fn skip(&mut self, mut count: i32) {
        if self.ramp_remaining > 0 {
            if count >= self.ramp_remaining {
                if self.target_volume == FADE_OUT {
                    self.target_volume = 0;
                    self.volume = 0;
                    self.left_volume = 0;
                    self.right_volume = 0;
                    self.unlink();
                    count = self.ramp_remaining;
                }
                self.ramp_remaining = 0;
                self.snap_to_target();
            } else {
                self.volume += self.volume_delta * count;
                self.left_volume += self.left_delta * count;
                self.right_volume += self.right_delta * count;
                self.ramp_remaining -= count;
            }
        }

        let start = (self.loop_start as i64) << self.frac_bits;
        let end = (self.loop_end as i64) << self.frac_bits;
        let length = self.fixed_length();
        let loop_length = end - start;
        if loop_length <= 0 {
            self.loops_remaining = 0;
        }
        if self.position < 0 {
            if self.step <= 0 {
                self.finish_ramp();
                self.unlink();
                return;
            }
            self.position = 0;
        }
        if self.position >= length {
            if self.step >= 0 {
                self.finish_ramp();
                self.unlink();
                return;
            }
            self.position = length - 1;
        }

        self.position = self
            .position
            .wrapping_add(self.step.wrapping_mul(count as i64));
        if self.loops_remaining >= 0 {
            if self.loops_remaining > 0 {
                if self.ping_pong {
                    'passes: {
                        if self.step < 0 {
                            if self.position >= start {
                                return;
                            }
                            self.position = start + start - self.position - 1;
                            self.step = -self.step;
                            self.looped = true;
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break 'passes;
                            }
                        }
                        loop {
                            if self.position < end {
                                return;
                            }
                            self.position = end + end - self.position - 1;
                            self.step = -self.step;
                            self.looped = true;
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break;
                            }
                            if self.position >= start {
                                return;
                            }
                            self.position = start + start - self.position - 1;
                            self.step = -self.step;
                            self.looped = true;
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break;
                            }
                        }
                    }
                } else if self.step < 0 {
                    if self.position >= start {
                        return;
                    }
                    let wraps = (end - self.position - 1) / loop_length;
                    if wraps < self.loops_remaining as i64 {
                        self.position += loop_length * wraps;
                        self.looped = true;
                        self.loops_remaining -= wraps as i32;
                        return;
                    }
                    self.position += loop_length * self.loops_remaining as i64;
                    self.looped = true;
                    self.loops_remaining = 0;
                } else {
                    if self.position < end {
                        return;
                    }
                    let wraps = (self.position - start) / loop_length;
                    if wraps < self.loops_remaining as i64 {
                        self.position -= loop_length * wraps;
                        self.looped = true;
                        self.loops_remaining -= wraps as i32;
                        return;
                    }
                    self.position -= loop_length * self.loops_remaining as i64;
                    self.looped = true;
                    self.loops_remaining = 0;
                }
            }
            if self.step < 0 {
                if self.position < 0 {
                    self.position = -1;
                    self.finish_ramp();
                    self.unlink();
                }
            } else if self.position >= length {
                self.position = length;
                self.finish_ramp();
                self.unlink();
            }
        } else if self.ping_pong {
            if self.step < 0 {
                if self.position >= start {
                    return;
                }
                self.position = start + start - self.position - 1;
                self.step = -self.step;
                self.looped = true;
            }
            while self.position >= end {
                self.position = end + end - self.position - 1;
                self.step = -self.step;
                self.looped = true;
                if self.position >= start {
                    return;
                }
                self.position = start + start - self.position - 1;
                self.step = -self.step;
                self.looped = true;
            }
        } else if self.step < 0 {
            if self.position < start {
                self.position = end - (end - 1 - self.position) % loop_length - 1;
                self.looped = true;
            }
        } else if self.position >= end {
            self.position = start + (self.position - start) % loop_length;
            self.looped = true;
        }
    }

    /// Mixes forwards from frame `frame` until the position reaches `end` (fixed point) or the
    /// output reaches `out_end`, handling any ramp in progress. `next_value` is the sample used
    /// to interpolate past `end` (the loop start when looping, otherwise 0). Returns the frame
    /// reached.
    fn mix_forward(
        &mut self,
        mix: &mut Mix,
        mut frame: i32,
        end: i64,
        out_end: i32,
        next_value: i32,
    ) -> i32 {
        loop {
            if self.ramp_remaining > 0 {
                let ramp_end = (frame + self.ramp_remaining).min(out_end);
                self.ramp_remaining += frame;
                frame = self.kernel_forward(mix, frame, ramp_end, end, next_value, true);
                self.ramp_remaining -= frame;
                if self.ramp_remaining != 0 {
                    return frame;
                }
                if !self.on_ramp_end() {
                    continue;
                }
                return out_end;
            }
            return self.kernel_forward(mix, frame, out_end, end, next_value, false);
        }
    }

    /// Backwards counterpart of [`Self::mix_forward`]; `previous_value` is the sample used to
    /// interpolate below `start`.
    fn mix_backward(
        &mut self,
        mix: &mut Mix,
        mut frame: i32,
        start: i64,
        out_end: i32,
        previous_value: i32,
    ) -> i32 {
        loop {
            if self.ramp_remaining > 0 {
                let ramp_end = (frame + self.ramp_remaining).min(out_end);
                self.ramp_remaining += frame;
                frame = self.kernel_backward(mix, frame, ramp_end, start, previous_value, true);
                self.ramp_remaining -= frame;
                if self.ramp_remaining != 0 {
                    return frame;
                }
                if !self.on_ramp_end() {
                    continue;
                }
                return out_end;
            }
            return self.kernel_backward(mix, frame, out_end, start, previous_value, false);
        }
    }

    /// Forward mixing kernel. Mixes frames `frame..frame_end` (stopping early at the position
    /// `end`) and returns the frame reached. With `ramp` set the volumes advance by their deltas
    /// every frame.
    fn kernel_forward(
        &mut self,
        mix: &mut Mix,
        mut frame: i32,
        frame_end: i32,
        end: i64,
        next_value: i32,
        ramp: bool,
    ) -> i32 {
        if mix.format.interpolation == Interpolation::Sinc {
            return self.kernel_sinc(mix, frame, frame_end, end, ramp);
        }
        let data = mix.data;
        let format = mix.format;
        let out = &mut *mix.out;
        let bits = self.frac_bits;
        let one = 1i64 << bits;
        if self.step == one && self.position & (one - 1) == 0 {
            // Step of exactly one sample on a sample boundary: no interpolation.
            let mut index = self.position >> bits;
            let limit = (frame as i64 + (end >> bits) - index).min(frame_end as i64) as i32;
            let shift = format.volume_shift();
            if format.stereo {
                let (mut left, mut right) = (self.left_volume << shift, self.right_volume << shift);
                let (left_step, right_step) = (self.left_delta << shift, self.right_delta << shift);
                if ramp {
                    self.volume += self.volume_delta * (limit - frame);
                }
                while frame < limit {
                    let value = data[index as usize] as i32;
                    index += 1;
                    let o = (frame * 2) as usize;
                    out[o] = out[o].wrapping_add(format.scale(value, left));
                    out[o + 1] = out[o + 1].wrapping_add(format.scale(value, right));
                    if ramp {
                        left = left.wrapping_add(left_step);
                        right = right.wrapping_add(right_step);
                    }
                    frame += 1;
                }
                if ramp {
                    self.left_volume = left >> shift;
                    self.right_volume = right >> shift;
                }
            } else {
                let mut volume = self.volume << shift;
                let volume_step = self.volume_delta << shift;
                if ramp {
                    self.left_volume += self.left_delta * (limit - frame);
                    self.right_volume += self.right_delta * (limit - frame);
                }
                while frame < limit {
                    let value = data[index as usize] as i32;
                    index += 1;
                    let o = frame as usize;
                    out[o] = out[o].wrapping_add(format.scale(value, volume));
                    if ramp {
                        volume = volume.wrapping_add(volume_step);
                    }
                    frame += 1;
                }
                if ramp {
                    self.volume = volume >> shift;
                }
            }
            self.position = index << bits;
            return frame;
        }

        // Interpolating path. The first phase interpolates between two samples inside the
        // region; the second handles the final sample before `end`, interpolating towards
        // `next_value`.
        let step = self.step;
        let mut position = self.position;
        let (mut volume, mut left, mut right) = (self.volume, self.left_volume, self.right_volume);
        if ramp {
            if format.stereo {
                self.volume -= self.volume_delta * frame;
            } else {
                self.left_volume -= self.left_delta * frame;
                self.right_volume -= self.right_delta * frame;
            }
        }
        for phase in 0..2 {
            let limit = if step == 0 {
                frame_end
            } else if phase == 0 {
                (frame as i64 + (end + step - position - one - 1) / step).min(frame_end as i64)
                    as i32
            } else {
                (frame as i64 + (end + step - position - 1) / step).min(frame_end as i64) as i32
            };
            while frame < limit {
                let index = (position >> bits) as usize;
                let current = data[index] as i32;
                let next = if phase == 0 {
                    data[index + 1] as i32
                } else {
                    next_value
                };
                let value = format.interpolate(current, next, position & (one - 1), bits);
                self.mix_value(
                    out,
                    frame,
                    value,
                    &mut volume,
                    &mut left,
                    &mut right,
                    format.stereo,
                    ramp,
                );
                position = position.wrapping_add(step);
                frame += 1;
            }
        }
        self.finish_interpolated_kernel(frame, position, volume, left, right, format.stereo, ramp);
        frame
    }

    /// Backward mixing kernel; see [`Self::kernel_forward`]. Mixes until the position drops
    /// below `start`.
    fn kernel_backward(
        &mut self,
        mix: &mut Mix,
        mut frame: i32,
        frame_end: i32,
        start: i64,
        previous_value: i32,
        ramp: bool,
    ) -> i32 {
        if mix.format.interpolation == Interpolation::Sinc {
            return self.kernel_sinc(mix, frame, frame_end, start, ramp);
        }
        let data = mix.data;
        let format = mix.format;
        let out = &mut *mix.out;
        let bits = self.frac_bits;
        let one = 1i64 << bits;
        if self.step == -one && self.position & (one - 1) == 0 {
            let mut index = self.position >> bits;
            let limit = (frame as i64 + index + 1 - (start >> bits)).min(frame_end as i64) as i32;
            let shift = format.volume_shift();
            if format.stereo {
                let (mut left, mut right) = (self.left_volume << shift, self.right_volume << shift);
                let (left_step, right_step) = (self.left_delta << shift, self.right_delta << shift);
                if ramp {
                    self.volume += self.volume_delta * (limit - frame);
                }
                while frame < limit {
                    let value = data[index as usize] as i32;
                    index -= 1;
                    let o = (frame * 2) as usize;
                    out[o] = out[o].wrapping_add(format.scale(value, left));
                    out[o + 1] = out[o + 1].wrapping_add(format.scale(value, right));
                    if ramp {
                        left = left.wrapping_add(left_step);
                        right = right.wrapping_add(right_step);
                    }
                    frame += 1;
                }
                if ramp {
                    self.left_volume = left >> shift;
                    self.right_volume = right >> shift;
                }
            } else {
                let mut volume = self.volume << shift;
                let volume_step = self.volume_delta << shift;
                if ramp {
                    self.left_volume += self.left_delta * (limit - frame);
                    self.right_volume += self.right_delta * (limit - frame);
                }
                while frame < limit {
                    let value = data[index as usize] as i32;
                    index -= 1;
                    let o = frame as usize;
                    out[o] = out[o].wrapping_add(format.scale(value, volume));
                    if ramp {
                        volume = volume.wrapping_add(volume_step);
                    }
                    frame += 1;
                }
                if ramp {
                    self.volume = volume >> shift;
                }
            }
            self.position = index << bits;
            return frame;
        }

        // Interpolating path. Going backwards, the value at position p interpolates between
        // samples floor(p) - 1 and floor(p); below the loop start it interpolates from
        // `previous_value`.
        let step = self.step;
        let mut position = self.position;
        let (mut volume, mut left, mut right) = (self.volume, self.left_volume, self.right_volume);
        if ramp {
            if format.stereo {
                self.volume -= self.volume_delta * frame;
            } else {
                self.left_volume -= self.left_delta * frame;
                self.right_volume -= self.right_delta * frame;
            }
        }
        for phase in 0..2 {
            let limit = if step == 0 {
                frame_end
            } else if phase == 0 {
                (frame as i64 + (start + step + one - position) / step).min(frame_end as i64) as i32
            } else {
                (frame as i64 + (start + step - position) / step).min(frame_end as i64) as i32
            };
            while frame < limit {
                let index = (position >> bits) as usize;
                let previous = if phase == 0 {
                    data[index - 1] as i32
                } else {
                    previous_value
                };
                let value =
                    format.interpolate(previous, data[index] as i32, position & (one - 1), bits);
                self.mix_value(
                    out,
                    frame,
                    value,
                    &mut volume,
                    &mut left,
                    &mut right,
                    format.stereo,
                    ramp,
                );
                position = position.wrapping_add(step);
                frame += 1;
            }
        }
        self.finish_interpolated_kernel(frame, position, volume, left, right, format.stereo, ramp);
        frame
    }

    /// Windowed-sinc mixing kernel for both directions. Mixes frames `frame..frame_end` while
    /// the position stays inside the region bounded by `bound` (below the `end` passed to
    /// [`Self::kernel_forward`] when playing forwards, at or above the `start` passed to
    /// [`Self::kernel_backward`] when playing backwards) and returns the frame reached.
    ///
    /// Like the linear kernels, playing backwards from position `p` reads the sample at `p - 1`.
    /// That makes ping-pong reflections (`p' = 2 * end - p - 1`) mirror the sample about
    /// half a sample before each loop boundary, which is what [`Self::tap`] assumes.
    fn kernel_sinc(
        &mut self,
        mix: &mut Mix,
        mut frame: i32,
        frame_end: i32,
        bound: i64,
        ramp: bool,
    ) -> i32 {
        let data = mix.data;
        let format = mix.format;
        let out = &mut *mix.out;
        let bits = self.frac_bits;
        let one = 1i64 << bits;
        let step = self.step;
        let limit = if step > 0 {
            (frame as i64 + (bound + step - self.position - 1) / step).min(frame_end as i64) as i32
        } else if step < 0 {
            (frame as i64 + (bound + step - self.position) / step).min(frame_end as i64) as i32
        } else {
            frame_end
        };

        let table = SincTable::for_ratio(step.unsigned_abs() as f64 / one as f64);
        let read_offset = if step < 0 { one } else { 0 };
        let phase_shift = bits - sinc::PHASE_BITS;
        let blend_scale = 1.0 / (1u64 << phase_shift) as f32;
        let value_scale = if format.wide { 1.0 } else { 256.0 };
        let (direct_start, direct_end) = self.direct_range();

        let mut position = self.position;
        let (mut volume, mut left, mut right) = (self.volume, self.left_volume, self.right_volume);
        if ramp {
            if format.stereo {
                self.volume -= self.volume_delta * frame;
            } else {
                self.left_volume -= self.left_delta * frame;
                self.right_volume -= self.right_delta * frame;
            }
        }
        while frame < limit {
            let at = position - read_offset;
            let fraction = at & (one - 1);
            let (a, b) = table.rows((fraction >> phase_shift) as usize);
            let blend = (fraction & ((1 << phase_shift) - 1)) as f32 * blend_scale;
            let first = (at >> bits) - table.half as i64 + 1;
            let (mut sum_a, mut sum_b) = (0f32, 0f32);
            if first >= direct_start && first + table.taps as i64 <= direct_end {
                let window = &data[first as usize..first as usize + table.taps];
                for i in 0..table.taps {
                    let x = window[i] as f32;
                    sum_a += x * a[i];
                    sum_b += x * b[i];
                }
            } else {
                for i in 0..table.taps {
                    let x = self.tap(data, first + i as i64);
                    sum_a += x * a[i];
                    sum_b += x * b[i];
                }
            }
            let value = ((sum_a + (sum_b - sum_a) * blend) * value_scale).round() as i32;
            self.mix_value(
                out,
                frame,
                value,
                &mut volume,
                &mut left,
                &mut right,
                format.stereo,
                ramp,
            );
            position = position.wrapping_add(step);
            frame += 1;
        }
        self.finish_interpolated_kernel(frame, position, volume, left, right, format.stereo, ramp);
        frame
    }

    /// Range of sample indices whose taps read the sample data directly in the current loop
    /// state; [`Self::tap`] is needed outside it.
    fn direct_range(&self) -> (i64, i64) {
        let length = self.sample.len() as i64;
        if self.loops_remaining == 0 {
            (0, length)
        } else {
            let start = if self.looped {
                self.loop_start as i64
            } else {
                0
            };
            (start.max(0), (self.loop_end as i64).min(length))
        }
    }

    /// The sample value the sinc kernel sees at `index`: the signal the voice actually plays,
    /// continued past the loop boundaries. While looping, indices at or past the loop end (and
    /// before the loop start once the voice has looped) wrap around the loop, or mirror about
    /// half a sample inside its boundaries for ping-pong loops. Everything else outside the
    /// sample is silence.
    fn tap(&self, data: &[i16], index: i64) -> f32 {
        let (start, end) = (self.loop_start as i64, self.loop_end as i64);
        let index = if self.loops_remaining != 0 && (index >= end || (index < start && self.looped))
        {
            let length = end - start;
            if self.ping_pong {
                let offset = (index - start).rem_euclid(2 * length);
                if offset < length {
                    start + offset
                } else {
                    start + 2 * length - 1 - offset
                }
            } else {
                start + (index - start).rem_euclid(length)
            }
        } else {
            index
        };
        if index < 0 {
            0.0
        } else {
            data.get(index as usize).map_or(0.0, |&value| value as f32)
        }
    }

    /// Mixes one interpolated value (scaled as returned by [`Mix::interpolate`]) into the
    /// output.
    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
    fn mix_value(
        &self,
        out: &mut [i32],
        frame: i32,
        value: i32,
        volume: &mut i32,
        left: &mut i32,
        right: &mut i32,
        stereo: bool,
        ramp: bool,
    ) {
        if stereo {
            let o = (frame * 2) as usize;
            out[o] = out[o].wrapping_add(value.wrapping_mul(*left) >> 6);
            out[o + 1] = out[o + 1].wrapping_add(value.wrapping_mul(*right) >> 6);
            if ramp {
                *left = left.wrapping_add(self.left_delta);
                *right = right.wrapping_add(self.right_delta);
            }
        } else {
            let o = frame as usize;
            out[o] = out[o].wrapping_add(value.wrapping_mul(*volume) >> 6);
            if ramp {
                *volume = volume.wrapping_add(self.volume_delta);
            }
        }
    }

    /// Stores state back after an interpolating kernel. During a ramp, the volumes not used
    /// for mixing (mono when stereo, left/right when mono) are advanced arithmetically.
    #[allow(clippy::too_many_arguments)]
    fn finish_interpolated_kernel(
        &mut self,
        frame: i32,
        position: i64,
        volume: i32,
        left: i32,
        right: i32,
        stereo: bool,
        ramp: bool,
    ) {
        if ramp {
            if stereo {
                self.volume += self.volume_delta * frame;
                self.left_volume = left;
                self.right_volume = right;
            } else {
                self.left_volume += self.left_delta * frame;
                self.right_volume += self.right_delta * frame;
                self.volume = volume;
            }
        }
        self.position = position;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BITS: u32 = 32;
    const AMPLITUDE: f64 = 16000.0;

    /// Plays `sample` mono at unity gain, `ratio` samples per output sample, and returns
    /// `count` output frames after skipping `skip`.
    fn render(
        sample: Sample,
        ratio: f64,
        interpolation: Interpolation,
        skip: usize,
        count: usize,
    ) -> Vec<f64> {
        let looped = sample.loop_end > sample.loop_start;
        let step = (ratio * (1u64 << BITS) as f64).round() as i64;
        // Volume 64 mixes 16-bit samples at unity.
        let mut voice = SampleVoice::new(Rc::new(sample), BITS, step, 64, -1).unwrap();
        if looped {
            voice.set_loops(-1);
        }
        let format = MixFormat {
            stereo: false,
            wide: true,
            interpolation,
        };
        let mut out = vec![0i32; skip + count];
        // Render in uneven chunks so loop boundaries fall inside calls.
        let mut offset = 0;
        while offset < out.len() {
            let chunk = 333.min(out.len() - offset);
            voice.read(&mut out, offset, chunk, format);
            offset += chunk;
        }
        out[skip..].iter().map(|&value| value as f64).collect()
    }

    /// Power of whatever isn't a sinusoid at `frequency` (cycles per frame), relative to the
    /// sinusoid, in dB. `signal` must span a whole number of cycles.
    fn residual_db(signal: &[f64], frequency: f64) -> f64 {
        let n = signal.len() as f64;
        let angle = |i: usize| 2.0 * std::f64::consts::PI * frequency * i as f64;
        let a: f64 = signal
            .iter()
            .enumerate()
            .map(|(i, x)| x * angle(i).sin())
            .sum::<f64>()
            * 2.0
            / n;
        let b: f64 = signal
            .iter()
            .enumerate()
            .map(|(i, x)| x * angle(i).cos())
            .sum::<f64>()
            * 2.0
            / n;
        let (mut fit_power, mut residual_power) = (0.0, 0.0);
        for (i, &x) in signal.iter().enumerate() {
            let fit = a * angle(i).sin() + b * angle(i).cos();
            fit_power += fit * fit;
            residual_power += (x - fit) * (x - fit);
        }
        10.0 * (residual_power / fit_power).log10()
    }

    fn sample(data: Vec<i16>, loop_start: i32, loop_end: i32, ping_pong: bool) -> Sample {
        Sample {
            sample_rate: 22050,
            data,
            loop_start,
            loop_end,
            ping_pong,
        }
    }

    /// A tone in a forward loop entered from silence, played two octaves down: after the first
    /// pass, the sinc kernel must see the loop's end before its start, not the silence.
    #[test]
    fn sinc_forward_loop_is_clean() {
        let period = 20;
        let data: Vec<i16> = (0..400)
            .map(|i| {
                if i < 200 {
                    0
                } else {
                    let phase = (i - 200) as f64 / period as f64;
                    (AMPLITUDE * (2.0 * std::f64::consts::PI * phase).sin()).round() as i16
                }
            })
            .collect();
        let measure = |interpolation| {
            let out = render(
                sample(data.clone(), 200, 400, false),
                0.25,
                interpolation,
                4000,
                8000,
            );
            residual_db(&out, 0.25 / period as f64)
        };
        let (linear, sinc) = (measure(Interpolation::Linear), measure(Interpolation::Sinc));
        println!(
            "forward loop residual: linear {:.1} dB, sinc {:.1} dB",
            linear, sinc
        );
        assert!(sinc < -85.0, "sinc residual {:.1} dB", sinc);
        assert!(sinc < linear - 30.0);
    }

    /// A ping-pong loop around a tone that is symmetric about both reflection points, so the
    /// played signal is a pure tone in both directions.
    #[test]
    fn sinc_ping_pong_loop_is_clean() {
        let (period, start, end) = (20.0, 100, 200);
        let data: Vec<i16> = (0..end)
            .map(|i| {
                if i < start {
                    0
                } else {
                    let phase = (i as f64 - (start as f64 - 0.5)) / period;
                    (AMPLITUDE * (2.0 * std::f64::consts::PI * phase).cos()).round() as i16
                }
            })
            .collect();
        let measure = |interpolation| {
            let out = render(
                sample(data.clone(), start, end, true),
                0.25,
                interpolation,
                4000,
                8000,
            );
            residual_db(&out, 0.25 / period)
        };
        let (linear, sinc) = (measure(Interpolation::Linear), measure(Interpolation::Sinc));
        println!(
            "ping-pong residual: linear {:.1} dB, sinc {:.1} dB",
            linear, sinc
        );
        assert!(sinc < -85.0, "sinc residual {:.1} dB", sinc);
        assert!(sinc < linear - 30.0);
    }

    /// A tone at 0.4 cycles per sample played 1.7 times faster lies above the output Nyquist
    /// frequency, so it must be filtered out instead of aliasing, while a tone at 0.1 cycles
    /// per sample passes.
    #[test]
    fn sinc_filters_content_above_output_nyquist() {
        let level = |frequency: f64, interpolation| {
            let data: Vec<i16> = (0..20000)
                .map(|i| {
                    let phase = frequency * i as f64;
                    (AMPLITUDE * (2.0 * std::f64::consts::PI * phase).sin()).round() as i16
                })
                .collect();
            let out = render(sample(data, 0, 0, false), 1.7, interpolation, 100, 10000);
            let power = out.iter().map(|x| x * x).sum::<f64>() / out.len() as f64;
            10.0 * (power / (AMPLITUDE * AMPLITUDE / 2.0)).log10()
        };
        let (linear, sinc) = (
            level(0.4, Interpolation::Linear),
            level(0.4, Interpolation::Sinc),
        );
        let passed = level(0.1, Interpolation::Sinc);
        println!(
            "aliased level: linear {:.1} dB, sinc {:.1} dB; in-band level {:.2} dB",
            linear, sinc, passed
        );
        assert!(sinc < -60.0, "sinc alias level {:.1} dB", sinc);
        assert!(passed.abs() < 0.1, "sinc in-band level {:.2} dB", passed);
    }
}
