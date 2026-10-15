//! Loading and caching of instrument samples.

use super::sample::Sample;
use super::synth::SynthSound;
use super::vorbis::{VorbisSample, VorbisSetup};
use crate::data::Data;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::rc::Rc;

/// Where an instrument sample is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleSource {
    /// Procedurally generated sound from archive 4.
    Synth,
    /// Vorbis-compressed recording from archive 14.
    Vorbis,
}

/// Decodes instrument samples on demand and caches them, so that every patch key referring to
/// the same sample shares one [`Sample`].
///
/// Sharing matters for more than memory: the output stage limits how many voices of the same
/// sample may sound at once (see [`MusicRenderer`](super::renderer::MusicRenderer)), and
/// identifies samples by identity.
pub struct SampleBank<'a> {
    data: &'a Data,
    /// Rate synth sounds are rendered at.
    synth_sample_rate: i32,
    vorbis_setup: Option<VorbisSetup>,
    samples: HashMap<(SampleSource, u32), Rc<Sample>>,
}

impl<'a> SampleBank<'a> {
    /// Creates an empty bank for playback at `output_rate`.
    pub fn new(data: &'a Data, output_rate: i32) -> Self {
        SampleBank {
            data,
            synth_sample_rate: data.engine.synth_sample_rate(output_rate),
            vorbis_setup: None,
            samples: HashMap::new(),
        }
    }

    /// Gets a sample, decoding it on first use.
    pub fn get(&mut self, source: SampleSource, id: u32) -> Result<Rc<Sample>> {
        if let Some(sample) = self.samples.get(&(source, id)) {
            return Ok(sample.clone());
        }
        let sample = Rc::new(match source {
            SampleSource::Synth => {
                let file = self
                    .data
                    .synth_archive
                    .get_by_id(id as usize)
                    .with_context(|| format!("Loading synth sample {}", id))?;
                SynthSound::decode(file)?.render(
                    self.synth_sample_rate,
                    self.data.engine.sixteen_bit_samples(),
                )?
            }
            SampleSource::Vorbis => {
                if self.vorbis_setup.is_none() {
                    let setup = self.data.vorbis_archive.get(0, 0)?;
                    self.vorbis_setup = Some(VorbisSetup::decode(&setup)?);
                }
                let file = self
                    .data
                    .vorbis_archive
                    .get_by_id(id as usize)
                    .with_context(|| format!("Loading Vorbis sample {}", id))?;
                VorbisSample::decode(file)?
                    .to_sample(self.vorbis_setup.as_ref().unwrap(), self.data.engine)
            }
        });
        self.samples.insert((source, id), sample.clone());
        Ok(sample)
    }
}
