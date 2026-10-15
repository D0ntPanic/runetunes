//! Resampling playback of a single [`Sample`].

use super::sample::Sample;
use std::rc::Rc;

/// Sentinel `target_volume` meaning "ramp to silence, then stop".
const FADE_OUT: i32 = i32::MIN;

/// Pan value for the centre (pan runs from 0 = left to 16384 = right).
pub const PAN_CENTER: i32 = 8192;

/// Plays a [`Sample`] at an arbitrary rate, mixing it into a 32-bit output buffer with linear
/// interpolation, volume/pan ramping and forward, reverse or ping-pong looping.
///
/// # Fixed-point conventions
///
/// * `position` is in 1/256 sample units (24.8 fixed point). A negative `step` plays the
///   sample backwards.
/// * With 8-bit samples a voice adds `(sample << 8) * volume >> 6` (that is,
///   `4 * sample * volume`) to the mix, and the output stage divides the mix by 256 to produce
///   16-bit audio. A volume of 16384 therefore plays a full-scale 8-bit sample at full 16-bit
///   scale. The 16-bit ("wide") kernels add `sample * volume >> 6`, the same scale for 16-bit
///   samples; see [`MixFormat`].
/// * Volume and pan changes are applied as linear ramps over a number of samples, tracked
///   separately for the mono volume and the left/right volumes.
#[derive(Debug, Clone)]
pub struct SampleVoice {
    pub sample: Rc<Sample>,
    /// Loop start/end copied from the sample, in samples.
    loop_start: i32,
    loop_end: i32,
    /// Whether looping alternates direction.
    ping_pong: bool,

    /// Playback position, 24.8 fixed point.
    position: i32,
    /// Position increment per output sample, 24.8 fixed point; negative when playing backwards.
    step: i32,
    /// Loop passes remaining: 0 = don't loop, -1 = loop forever.
    loops_remaining: i32,

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
}

impl MixFormat {
    /// Interpolates between two samples at `fraction` (0..256) of the way from `from` to `to`.
    #[inline(always)]
    fn interpolate(&self, from: i32, to: i32, fraction: i32) -> i32 {
        if self.wide {
            from + (((to - from) * fraction) >> 8)
        } else {
            (from << 8) + (to - from) * fraction
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
    /// Starts a voice at the beginning of `sample`. Returns `None` for an empty sample.
    pub fn new(sample: Rc<Sample>, step: i32, volume: i32, pan: i32) -> Option<Self> {
        if sample.data.is_empty() {
            return None;
        }
        let mut voice = SampleVoice {
            loop_start: sample.loop_start,
            loop_end: sample.loop_end,
            ping_pong: sample.ping_pong,
            sample,
            position: 0,
            step,
            loops_remaining: 0,
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

    /// Current playback rate (always positive).
    pub fn rate(&self) -> i32 {
        self.step.abs()
    }

    /// Sets the playback rate, keeping the current direction.
    pub fn set_rate(&mut self, rate: i32) {
        self.step = if self.step < 0 { -rate } else { rate };
    }

    /// Makes the voice play backwards.
    pub fn play_backwards(&mut self) {
        self.step = -self.step.abs();
    }

    /// Moves the playback position (24.8 fixed point), clamped to just outside the sample.
    pub fn set_position(&mut self, position: i32) {
        let end = self.sample.len() << 8;
        self.position = position.clamp(-1, end);
    }

    /// Whether the position has run off either end of the sample.
    pub fn is_finished(&self) -> bool {
        self.position < 0 || self.position >= self.sample.len() << 8
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
            priority -= (priority.wrapping_mul(self.position)) / (self.sample.len() << 8);
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
        let start = self.loop_start << 8;
        let end = self.loop_end << 8;
        let length = sample.len() << 8;
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
                        if wraps >= self.loops_remaining {
                            self.position += loop_length * self.loops_remaining;
                            self.loops_remaining = 0;
                            break;
                        }
                        self.position += loop_length * wraps;
                        self.loops_remaining -= wraps;
                    }
                } else {
                    loop {
                        frame = self.mix_forward(&mut mix, frame, end, out_end, loop_first);
                        if self.position < end {
                            return;
                        }
                        let wraps = (self.position - start) / loop_length;
                        if wraps >= self.loops_remaining {
                            self.position -= loop_length * self.loops_remaining;
                            self.loops_remaining = 0;
                            break;
                        }
                        self.position -= loop_length * wraps;
                        self.loops_remaining -= wraps;
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
            }
            loop {
                frame = self.mix_forward(&mut mix, frame, end, out_end, loop_last());
                if self.position < end {
                    return;
                }
                self.position = end + end - self.position - 1;
                self.step = -self.step;
                frame = self.mix_backward(&mut mix, frame, start, out_end, loop_first);
                if self.position >= start {
                    return;
                }
                self.position = start + start - self.position - 1;
                self.step = -self.step;
            }
        } else if self.step < 0 {
            loop {
                frame = self.mix_backward(&mut mix, frame, start, out_end, loop_last());
                if self.position >= start {
                    return;
                }
                self.position = end - (end - 1 - self.position) % loop_length - 1;
            }
        } else {
            loop {
                frame = self.mix_forward(&mut mix, frame, end, out_end, loop_first);
                if self.position < end {
                    return;
                }
                self.position = start + (self.position - start) % loop_length;
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

        let start = self.loop_start << 8;
        let end = self.loop_end << 8;
        let length = self.sample.len() << 8;
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

        self.position = self.position.wrapping_add(self.step.wrapping_mul(count));
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
                            self.loops_remaining -= 1;
                            if self.loops_remaining == 0 {
                                break;
                            }
                            if self.position >= start {
                                return;
                            }
                            self.position = start + start - self.position - 1;
                            self.step = -self.step;
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
                    if wraps < self.loops_remaining {
                        self.position += loop_length * wraps;
                        self.loops_remaining -= wraps;
                        return;
                    }
                    self.position += loop_length * self.loops_remaining;
                    self.loops_remaining = 0;
                } else {
                    if self.position < end {
                        return;
                    }
                    let wraps = (self.position - start) / loop_length;
                    if wraps < self.loops_remaining {
                        self.position -= loop_length * wraps;
                        self.loops_remaining -= wraps;
                        return;
                    }
                    self.position -= loop_length * self.loops_remaining;
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
            }
            while self.position >= end {
                self.position = end + end - self.position - 1;
                self.step = -self.step;
                if self.position >= start {
                    return;
                }
                self.position = start + start - self.position - 1;
                self.step = -self.step;
            }
        } else if self.step < 0 {
            if self.position < start {
                self.position = end - (end - 1 - self.position) % loop_length - 1;
            }
        } else if self.position >= end {
            self.position = start + (self.position - start) % loop_length;
        }
    }

    /// Mixes forwards from frame `frame` until the position reaches `end` (24.8) or the output
    /// reaches `out_end`, handling any ramp in progress. `next_value` is the sample used to
    /// interpolate past `end` (the loop start when looping, otherwise 0). Returns the frame
    /// reached.
    fn mix_forward(
        &mut self,
        mix: &mut Mix,
        mut frame: i32,
        end: i32,
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
        start: i32,
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
        end: i32,
        next_value: i32,
        ramp: bool,
    ) -> i32 {
        let data = mix.data;
        let format = mix.format;
        let out = &mut *mix.out;
        if self.step == 256 && self.position & 0xff == 0 {
            // Step of exactly one sample on a sample boundary: no interpolation.
            let mut index = self.position >> 8;
            let limit = (frame + (end >> 8) - index).min(frame_end);
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
            self.position = index << 8;
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
                (frame + (end + step - position - 257) / step).min(frame_end)
            } else {
                (frame + (end + step - position - 1) / step).min(frame_end)
            };
            while frame < limit {
                let index = (position >> 8) as usize;
                let current = data[index] as i32;
                let next = if phase == 0 {
                    data[index + 1] as i32
                } else {
                    next_value
                };
                let value = format.interpolate(current, next, position & 0xff);
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
        start: i32,
        previous_value: i32,
        ramp: bool,
    ) -> i32 {
        let data = mix.data;
        let format = mix.format;
        let out = &mut *mix.out;
        if self.step == -256 && self.position & 0xff == 0 {
            let mut index = self.position >> 8;
            let limit = (frame + index + 1 - (start >> 8)).min(frame_end);
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
            self.position = index << 8;
            return frame;
        }

        // Interpolating path. Going backwards, the value at position p interpolates between
        // samples (p >> 8) - 1 and (p >> 8); below the loop start it interpolates from
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
                (frame + (start + step + 256 - position) / step).min(frame_end)
            } else {
                (frame + (start + step - position) / step).min(frame_end)
            };
            while frame < limit {
                let index = (position >> 8) as usize;
                let previous = if phase == 0 {
                    data[index - 1] as i32
                } else {
                    previous_value
                };
                let value = format.interpolate(previous, data[index] as i32, position & 0xff);
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
        position: i32,
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
