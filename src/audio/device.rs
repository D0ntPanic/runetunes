//! Real-time playback on an audio device with `cpal`.

use super::renderer::{to_i16, MusicRenderer};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Amount of audio buffered ahead of the device, in seconds.
const BUFFER_SECONDS: f64 = 0.2;

/// Silence played after the song ends, in seconds.
const TAIL_SECONDS: f64 = 1.0;

/// Plays a renderer on the default output device until the song ends.
///
/// The renderer stays on the calling thread (it is not `Send`) and fills a small ring buffer
/// that the device callback drains. If the device does not support the synthesizer's sample
/// rate, which is likely for the default 22050 Hz, the output is resampled to the device rate.
pub fn play(mut renderer: MusicRenderer) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("No audio output device found"))?;

    let source_rate = renderer.sample_rate() as u32;
    let source_channels = renderer.channels();

    // Prefer a configuration at the synth's own rate so no resampling is needed.
    let native = device.supported_output_configs()?.find_map(|range| {
        if range.channels() as usize >= source_channels
            && matches!(range.sample_format(), SampleFormat::F32 | SampleFormat::I16)
        {
            range.try_with_sample_rate(source_rate)
        } else {
            None
        }
    });
    let config = match native {
        Some(config) => config,
        None => device.default_output_config()?,
    };
    let format = config.sample_format();
    let config: StreamConfig = config.into();

    let buffer = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let underrun = Arc::new(AtomicBool::new(false));
    let stream = match format {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, &buffer, &underrun)?,
        SampleFormat::I16 => build_stream::<i16>(&device, &config, &buffer, &underrun)?,
        SampleFormat::I32 => build_stream::<i32>(&device, &config, &buffer, &underrun)?,
        SampleFormat::U16 => build_stream::<u16>(&device, &config, &buffer, &underrun)?,
        SampleFormat::F64 => build_stream::<f64>(&device, &config, &buffer, &underrun)?,
        format => return Err(anyhow!("Unsupported device sample format {}", format)),
    };
    stream.play()?;

    let device_rate = config.sample_rate;
    let device_channels = config.channels as usize;
    let mut resampler = Resampler::new(source_rate, device_rate, source_channels);
    let capacity = (BUFFER_SECONDS * device_rate as f64) as usize * device_channels;
    let chunk_seconds = renderer.chunk_frames() as f64 / source_rate as f64;
    let mut tail_chunks = (TAIL_SECONDS / chunk_seconds).ceil() as u32;
    let mut converted = Vec::new();
    let mut resampled = Vec::new();

    while tail_chunks > 0 {
        if !renderer.synth.is_active() {
            tail_chunks -= 1;
        }
        converted.clear();
        converted.extend(
            renderer
                .render_chunk()
                .iter()
                .map(|&sample| to_i16(sample) as f32 / 32768.0),
        );
        resampled.clear();
        resampler.process(&converted, &mut resampled);

        // Wait for room in the device buffer, then queue the frames, mapping channels.
        loop {
            let queued = buffer.lock().unwrap().len();
            if queued + resampled.len() / source_channels * device_channels <= capacity.max(1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut queue = buffer.lock().unwrap();
        for frame in resampled.chunks(source_channels) {
            for channel in 0..device_channels {
                queue.push_back(if source_channels == 1 {
                    frame[0]
                } else if channel < source_channels {
                    frame[channel]
                } else {
                    0.0
                });
            }
        }
    }

    // Let the device drain what is queued.
    while !buffer.lock().unwrap().is_empty() {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(100));
    if underrun.load(Ordering::Relaxed) {
        eprintln!("Warning: audio output underran (the renderer could not keep up)");
    }
    Ok(())
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    buffer: &Arc<Mutex<VecDeque<f32>>>,
    underrun: &Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let buffer = buffer.clone();
    let underrun = underrun.clone();
    let started = Arc::new(AtomicBool::new(false));
    let stream = device.build_output_stream(
        *config,
        move |output: &mut [T], _| {
            let mut queue = buffer.lock().unwrap();
            if !queue.is_empty() {
                started.store(true, Ordering::Relaxed);
            }
            for sample in output.iter_mut() {
                *sample = T::from_sample(queue.pop_front().unwrap_or_else(|| {
                    if started.load(Ordering::Relaxed) {
                        underrun.store(true, Ordering::Relaxed);
                    }
                    0.0
                }));
            }
        },
        |error| eprintln!("Audio stream error: {}", error),
        None,
    )?;
    Ok(stream)
}

/// Streaming sample rate converter using a windowed-sinc filter.
struct Resampler {
    channels: usize,
    /// Input samples advanced per output sample.
    step: f64,
    /// Position of the next output sample, in input samples relative to `history[0]`.
    position: f64,
    /// Buffered input, per channel. Starts with `HALF_TAPS` samples of silence.
    history: Vec<Vec<f32>>,
    /// Filter coefficients for each of `PHASES + 1` fractional offsets.
    table: Vec<[f32; TAPS]>,
    /// Pass-through when the rates match.
    bypass: bool,
}

const HALF_TAPS: usize = 16;
const TAPS: usize = HALF_TAPS * 2;
const PHASES: usize = 256;

impl Resampler {
    fn new(from: u32, to: u32, channels: usize) -> Self {
        let step = from as f64 / to as f64;
        // Low-pass below the lower of the two Nyquist frequencies.
        let cutoff = 0.95 * (to as f64 / from as f64).min(1.0);
        let table = (0..=PHASES)
            .map(|phase| {
                let fraction = phase as f64 / PHASES as f64;
                let mut taps = [0f32; TAPS];
                for (i, tap) in taps.iter_mut().enumerate() {
                    // Tap i applies to input sample floor(position) - HALF_TAPS + 1 + i.
                    let x = i as f64 - (HALF_TAPS as f64 - 1.0) - fraction;
                    let sinc = if x.abs() < 1e-9 {
                        1.0
                    } else {
                        (std::f64::consts::PI * cutoff * x).sin()
                            / (std::f64::consts::PI * cutoff * x)
                    };
                    // Blackman window over [-HALF_TAPS, HALF_TAPS].
                    let w = (x + HALF_TAPS as f64) / TAPS as f64;
                    let window = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * w).cos()
                        + 0.08 * (4.0 * std::f64::consts::PI * w).cos();
                    *tap = (cutoff * sinc * window) as f32;
                }
                taps
            })
            .collect();
        Resampler {
            channels,
            step,
            position: HALF_TAPS as f64 - 1.0,
            history: vec![vec![0.0; HALF_TAPS]; channels],
            table,
            bypass: from == to,
        }
    }

    /// Converts interleaved `input` frames, appending interleaved output frames to `output`.
    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.bypass {
            output.extend_from_slice(input);
            return;
        }
        for frame in input.chunks(self.channels) {
            for (channel, &sample) in frame.iter().enumerate() {
                self.history[channel].push(sample);
            }
        }
        let available = self.history[0].len();
        while (self.position.floor() as usize) + HALF_TAPS < available {
            let base = self.position.floor() as usize + 1 - HALF_TAPS;
            let phase = (self.position.fract() * PHASES as f64) as usize;
            let mix = (self.position.fract() * PHASES as f64).fract() as f32;
            let (a, b) = (&self.table[phase], &self.table[phase + 1]);
            for channel in 0..self.channels {
                let history = &self.history[channel][base..base + TAPS];
                let mut sum = 0.0f32;
                for i in 0..TAPS {
                    sum += history[i] * (a[i] + (b[i] - a[i]) * mix);
                }
                output.push(sum);
            }
            self.position += self.step;
        }
        // Drop input that no future output sample needs.
        let consumed = (self.position.floor() as usize + 1).saturating_sub(HALF_TAPS);
        if consumed > 0 {
            for history in self.history.iter_mut() {
                history.drain(..consumed);
            }
            self.position -= consumed as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    /// Resamples a sine wave and checks the output length, amplitude and frequency.
    #[test]
    fn resampler_preserves_sine() {
        let (from, to) = (22050u32, 48000u32);
        let frequency = 1000.0;
        let input: Vec<f32> = (0..from as usize)
            .map(|i| {
                (2.0 * std::f64::consts::PI * frequency * i as f64 / from as f64).sin() as f32 * 0.5
            })
            .collect();
        let mut resampler = Resampler::new(from, to, 1);
        let mut output = Vec::new();
        for chunk in input.chunks(256) {
            resampler.process(chunk, &mut output);
        }

        let expected_len = to as usize;
        assert!(
            (output.len() as i64 - expected_len as i64).abs() < 64,
            "length {}",
            output.len()
        );

        // Output sample i is centred on input sample i * step - 1 (the history starts with
        // HALF_TAPS samples of silence and the first output is at position HALF_TAPS - 1).
        let step = from as f64 / to as f64;
        let mut max_error = 0.0f64;
        for (i, &value) in output.iter().enumerate().skip(1000).take(40000) {
            let t = (i as f64 * step - 1.0) / from as f64;
            let ideal = (2.0 * std::f64::consts::PI * frequency * t).sin() * 0.5;
            max_error = max_error.max((value as f64 - ideal).abs());
        }
        assert!(max_error < 0.01, "max error {}", max_error);
    }

    #[test]
    fn resampler_bypasses_matching_rates() {
        let mut resampler = Resampler::new(22050, 22050, 2);
        let mut output = Vec::new();
        resampler.process(&[0.25, -0.25, 0.5, -0.5], &mut output);
        assert_eq!(output, vec![0.25, -0.25, 0.5, -0.5]);
    }
}
